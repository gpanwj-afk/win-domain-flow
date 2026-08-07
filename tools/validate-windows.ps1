[CmdletBinding()]
param(
    [string]$BinaryRoot = "",
    [string]$OutputDirectory = "",
    [switch]$CiMode
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$RepoRoot = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($BinaryRoot)) {
    $BinaryRoot = Join-Path $RepoRoot "target\release"
}
$BinaryRoot = [IO.Path]::GetFullPath($BinaryRoot)
$Cli = Join-Path $BinaryRoot "win-domain-flow.exe"
$ExtensionDir = Join-Path $RepoRoot "browser-extension"
$FixtureScript = Join-Path $PSScriptRoot "browser_fixture.py"
$QueryScript = Join-Path $PSScriptRoot "query_e2e_db.py"

$RunId = [Guid]::NewGuid().ToString("N")
$RunRoot = Join-Path ([IO.Path]::GetTempPath()) "win-domain-flow-e2e-$RunId"
$ProfileDir = Join-Path $RunRoot "edge-profile"
$DownloadDir = Join-Path $RunRoot "downloads"
$TestDb = Join-Path $RunRoot "validation.db"
if ([string]::IsNullOrWhiteSpace($OutputDirectory)) {
    $OutputDirectory = Join-Path $RunRoot "report"
}
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
New-Item -ItemType Directory -Path $RunRoot, $ProfileDir, $DownloadDir, $OutputDirectory -Force | Out-Null

$Results = [Collections.Generic.List[object]]::new()
$OwnedTargets = [Collections.Generic.List[string]]::new()
$BrowserSocket = $null
$ServiceSession = $null
$PreviousDiagnosticsEnabled = $false
$FixtureProcess = $null
$ReceiverProcess = $null
$BrowserRootProcess = $null
$Fixture = $null
$ReceiverStatus = $null
$ExtensionId = $null
$BrowserDebugPort = $null
$BrowserExe = $null
$BrowserProcessName = $null
$FatalMessage = $null
$script:CdpRequestId = 0

function Add-Result {
    param([string]$Name, [string]$Status, [string]$Evidence = "")
    $Results.Add([pscustomobject]@{
        name = $Name
        status = $Status
        evidence = $Evidence
    }) | Out-Null
}

function Assert-Evidence {
    param([bool]$Condition, [string]$Name, [string]$Evidence)
    if (-not $Condition) {
        Add-Result $Name "FAIL" $Evidence
        throw "$Name: $Evidence"
    }
    Add-Result $Name "PASS" $Evidence
}

function Quote-Argument {
    param([string]$Value)
    if ($Value -notmatch '[\s"]') { return $Value }
    return '"' + ($Value -replace '(\\*)"', '$1$1\"' -replace '(\\+)$', '$1$1') + '"'
}

function Start-ToolProcess {
    param(
        [string]$FilePath,
        [string[]]$ArgumentList,
        [string]$WorkingDirectory,
        [switch]$RedirectOutput
    )
    $psi = [Diagnostics.ProcessStartInfo]::new()
    $psi.FileName = $FilePath
    $psi.WorkingDirectory = $WorkingDirectory
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $RedirectOutput.IsPresent
    $hasArgumentList = $psi.PSObject.Properties.Name -contains "ArgumentList"
    if ($hasArgumentList) {
        foreach ($argument in $ArgumentList) { $psi.ArgumentList.Add($argument) }
    } else {
        $psi.Arguments = (($ArgumentList | ForEach-Object { Quote-Argument $_ }) -join " ")
    }
    if ($RedirectOutput) {
        $psi.RedirectStandardOutput = $true
        $psi.RedirectStandardError = $true
    }
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $psi
    if (-not $process.Start()) { throw "Failed to start $FilePath" }
    return $process
}

function Stop-ExactProcess {
    param([Diagnostics.Process]$Process, [string]$Label)
    if ($null -eq $Process) { return }
    try { $Process.Refresh() } catch { return }
    if ($Process.HasExited) { return }
    try {
        $Process.Kill()
        if (-not $Process.WaitForExit(8000)) {
            throw "$Label PID $($Process.Id) did not exit within 8 seconds"
        }
    } catch {
        throw "Failed to stop owned $Label PID $($Process.Id): $($_.Exception.Message)"
    }
}

function Get-Python {
    foreach ($name in @("python.exe", "python", "py.exe", "py")) {
        $command = Get-Command $name -ErrorAction SilentlyContinue
        if ($command) { return $command.Source }
    }
    throw "Python was not found"
}

function Get-BrowserExecutable {
    $candidates = @(
        "$env:ProgramFiles(x86)\Microsoft\Edge\Application\msedge.exe",
        "$env:ProgramFiles\Microsoft\Edge\Application\msedge.exe",
        "$env:LOCALAPPDATA\Microsoft\Edge\Application\msedge.exe",
        "$env:ProgramFiles\Google\Chrome\Application\chrome.exe",
        "$env:ProgramFiles(x86)\Google\Chrome\Application\chrome.exe"
    ) | Where-Object { $_ -and (Test-Path $_) }
    if ($candidates.Count -eq 0) { throw "Edge/Chrome executable was not found" }
    return [IO.Path]::GetFullPath($candidates[0])
}

function Wait-FileLines {
    param([string]$Path, [int]$MinimumLines, [int]$TimeoutSeconds)
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    while ([DateTime]::UtcNow -lt $deadline) {
        if (Test-Path $Path) {
            try {
                $lines = @(Get-Content -LiteralPath $Path -ErrorAction Stop)
                if ($lines.Count -ge $MinimumLines) { return $lines }
            } catch { }
        }
        Start-Sleep -Milliseconds 100
    }
    throw "Timed out waiting for $Path"
}

function Connect-Cdp {
    param([string]$Uri)
    $socket = [Net.WebSockets.ClientWebSocket]::new()
    $socket.ConnectAsync([Uri]$Uri, [Threading.CancellationToken]::None).GetAwaiter().GetResult()
    return $socket
}

function Receive-CdpMessage {
    param([Net.WebSockets.ClientWebSocket]$Socket)
    $memory = [IO.MemoryStream]::new()
    $buffer = New-Object byte[] 65536
    try {
        do {
            $segment = [ArraySegment[byte]]::new($buffer)
            $result = $Socket.ReceiveAsync($segment, [Threading.CancellationToken]::None).GetAwaiter().GetResult()
            if ($result.MessageType -eq [Net.WebSockets.WebSocketMessageType]::Close) {
                throw "CDP WebSocket closed unexpectedly"
            }
            $memory.Write($buffer, 0, $result.Count)
        } while (-not $result.EndOfMessage)
        return [Text.Encoding]::UTF8.GetString($memory.ToArray()) | ConvertFrom-Json -Depth 100
    } finally {
        $memory.Dispose()
    }
}

function Send-Cdp {
    param(
        [Net.WebSockets.ClientWebSocket]$Socket,
        [string]$Method,
        [hashtable]$Params = @{},
        [string]$SessionId = ""
    )
    $script:CdpRequestId++
    $id = $script:CdpRequestId
    $payload = [ordered]@{ id = $id; method = $Method; params = $Params }
    if (-not [string]::IsNullOrWhiteSpace($SessionId)) { $payload.sessionId = $SessionId }
    $json = $payload | ConvertTo-Json -Depth 50 -Compress
    $bytes = [Text.Encoding]::UTF8.GetBytes($json)
    $Socket.SendAsync(
        [ArraySegment[byte]]::new($bytes),
        [Net.WebSockets.WebSocketMessageType]::Text,
        $true,
        [Threading.CancellationToken]::None
    ).GetAwaiter().GetResult()

    while ($true) {
        $message = Receive-CdpMessage $Socket
        $idProperty = $message.PSObject.Properties["id"]
        if ($null -eq $idProperty -or [int]$idProperty.Value -ne $id) { continue }
        $errorProperty = $message.PSObject.Properties["error"]
        if ($null -ne $errorProperty -and $null -ne $errorProperty.Value) {
            throw "CDP $Method failed: $($errorProperty.Value | ConvertTo-Json -Compress)"
        }
        $resultProperty = $message.PSObject.Properties["result"]
        if ($null -eq $resultProperty) { return $null }
        return $resultProperty.Value
    }
}

function Evaluate-Cdp {
    param(
        [Net.WebSockets.ClientWebSocket]$Socket,
        [string]$SessionId,
        [string]$Expression
    )
    $result = Send-Cdp $Socket "Runtime.evaluate" @{
        expression = $Expression
        awaitPromise = $true
        returnByValue = $true
    } $SessionId
    $exceptionProperty = $result.PSObject.Properties["exceptionDetails"]
    if ($null -ne $exceptionProperty -and $null -ne $exceptionProperty.Value) {
        throw "Runtime.evaluate failed: $($exceptionProperty.Value | ConvertTo-Json -Compress)"
    }
    return $result.result.value
}

function Find-ExtensionTarget {
    param([Net.WebSockets.ClientWebSocket]$Socket, [int]$TimeoutSeconds)
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    while ([DateTime]::UtcNow -lt $deadline) {
        $targets = (Send-Cdp $Socket "Target.getTargets" @{}).targetInfos
        foreach ($target in $targets) {
            if ($target.type -notin @("service_worker", "background_page")) { continue }
            if ($target.url -match '^(chrome|edge)-extension://([a-p]{32})/') {
                return [pscustomobject]@{
                    target = $target
                    extensionId = $Matches[2]
                }
            }
        }
        Start-Sleep -Milliseconds 250
    }
    return $null
}

function Get-OwnedBrowserProcesses {
    param([string]$Profile, [string]$ProcessName)
    return @(
        Get-CimInstance Win32_Process -Filter "Name='$ProcessName'" -ErrorAction SilentlyContinue |
            Where-Object { $_.CommandLine -and $_.CommandLine.Contains($Profile, [StringComparison]::OrdinalIgnoreCase) }
    )
}

function Stop-OwnedBrowserProcesses {
    param([string]$Profile, [string]$ProcessName)
    $owned = @(Get-OwnedBrowserProcesses $Profile $ProcessName)
    foreach ($item in ($owned | Sort-Object ProcessId -Descending)) {
        try {
            Stop-Process -Id ([int]$item.ProcessId) -Force -ErrorAction Stop
        } catch {
            throw "Failed to stop owned browser PID $($item.ProcessId): $($_.Exception.Message)"
        }
    }
    $deadline = [DateTime]::UtcNow.AddSeconds(8)
    do {
        $remaining = @(Get-OwnedBrowserProcesses $Profile $ProcessName)
        if ($remaining.Count -eq 0) { return }
        Start-Sleep -Milliseconds 150
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Owned browser processes remain for profile $Profile: $($remaining.ProcessId -join ',')"
}

function Query-TestDatabase {
    param([string]$Python, [string]$Database)
    $json = & $Python $QueryScript $Database
    if ($LASTEXITCODE -ne 0) { throw "Read-only database query failed" }
    return ($json -join "`n") | ConvertFrom-Json -Depth 100
}

function Write-Reports {
    param([string]$OverallStatus, [hashtable]$Manifest, [object]$DatabaseEvidence)
    $report = [ordered]@{
        status = $OverallStatus
        generated_at_utc = [DateTime]::UtcNow.ToString("o")
        results = @($Results)
        manifest = $Manifest
        database_evidence = $DatabaseEvidence
        fatal_error = $FatalMessage
    }
    $jsonPath = Join-Path $OutputDirectory "validation-report.json"
    $report | ConvertTo-Json -Depth 100 | Set-Content -LiteralPath $jsonPath -Encoding UTF8

    $failures = @($Results | Where-Object { $_.status -eq "FAIL" }).Count
    $skipped = @($Results | Where-Object { $_.status -in @("NOT_RUN", "EVIDENCE_INSUFFICIENT") }).Count
    $cases = foreach ($item in $Results) {
        $name = [Security.SecurityElement]::Escape([string]$item.name)
        $evidence = [Security.SecurityElement]::Escape([string]$item.evidence)
        if ($item.status -eq "FAIL") {
            "  <testcase name=`"$name`"><failure message=`"$evidence`" /></testcase>"
        } elseif ($item.status -in @("NOT_RUN", "EVIDENCE_INSUFFICIENT")) {
            "  <testcase name=`"$name`"><skipped message=`"$evidence`" /></testcase>"
        } else {
            "  <testcase name=`"$name`" />"
        }
    }
    $xml = @(
        "<?xml version=`"1.0`" encoding=`"utf-8`"?>",
        "<testsuite name=`"win-domain-flow-windows-e2e`" tests=`"$($Results.Count)`" failures=`"$failures`" skipped=`"$skipped`">",
        $cases,
        "</testsuite>"
    ) -join "`n"
    Set-Content -LiteralPath (Join-Path $OutputDirectory "validation-report.xml") -Value $xml -Encoding UTF8
    $Manifest | ConvertTo-Json -Depth 30 | Set-Content -LiteralPath (Join-Path $OutputDirectory "validation-manifest.json") -Encoding UTF8
    Write-Host "Validation report: $jsonPath"
}

$Python = Get-Python
$DatabaseEvidence = $null
$Manifest = @{
    commit = $null
    binary_sha256 = $null
    cli_path = $Cli
    database_path = $TestDb
    profile_path = $ProfileDir
    extension_id = $null
    expected_extension_id = $null
    fixture_pid = $null
    fixture_port = $null
    receiver_pid_initial = $null
    receiver_pid_restart = $null
    receiver_port = $null
    browser_root_pid = $null
    browser_owned_pids = @()
    browser_executable = $null
    browser_debug_port = $null
}

try {
    Assert-Evidence (Test-Path $Cli) "CLI executable exists" $Cli
    Assert-Evidence (Test-Path (Join-Path $ExtensionDir "manifest.json")) "Browser extension exists" $ExtensionDir
    Assert-Evidence (Test-Path $FixtureScript) "Local fixture exists" $FixtureScript

    try {
        $Manifest.commit = (& git -C $RepoRoot rev-parse HEAD 2>$null).Trim()
    } catch { $Manifest.commit = "unknown" }
    $Manifest.binary_sha256 = (Get-FileHash -LiteralPath $Cli -Algorithm SHA256).Hash.ToLowerInvariant()

    $FixtureProcess = Start-ToolProcess $Python @($FixtureScript, "--port", "0") $RepoRoot -RedirectOutput
    $fixtureLine = $FixtureProcess.StandardOutput.ReadLine()
    if ([string]::IsNullOrWhiteSpace($fixtureLine)) {
        throw "Fixture did not emit startup JSON: $($FixtureProcess.StandardError.ReadToEnd())"
    }
    $Fixture = $fixtureLine | ConvertFrom-Json
    $Manifest.fixture_pid = $FixtureProcess.Id
    $Manifest.fixture_port = [int]$Fixture.port
    Assert-Evidence ($FixtureProcess.Id -eq [int]$Fixture.pid) "Fixture PID ownership" "PID $($FixtureProcess.Id)"
    $fixtureHealth = Invoke-RestMethod -Uri "http://127.0.0.1:$($Fixture.port)/health" -TimeoutSec 3
    Assert-Evidence ([bool]$fixtureHealth.ok) "Local fixture health" "127.0.0.1:$($Fixture.port)"

    $ReceiverProcess = Start-ToolProcess $Cli @("browser-receiver", "--db", $TestDb, "--port", "0") $RepoRoot -RedirectOutput
    $receiverLine = $ReceiverProcess.StandardOutput.ReadLine()
    if ([string]::IsNullOrWhiteSpace($receiverLine)) {
        throw "Receiver did not emit startup status: $($ReceiverProcess.StandardError.ReadToEnd())"
    }
    $receiverFields = @{}
    foreach ($field in ($receiverLine -split "`t")) {
        $parts = $field.Split('=', 2)
        if ($parts.Count -eq 2) { $receiverFields[$parts[0]] = $parts[1] }
    }
    $ReceiverPort = [int]$receiverFields.port
    $Manifest.receiver_pid_initial = $ReceiverProcess.Id
    $Manifest.receiver_port = $ReceiverPort
    Assert-Evidence ($ReceiverProcess.Id -eq [int]$receiverFields.pid) "Receiver PID ownership" "PID $($ReceiverProcess.Id)"

    $ReceiverStatus = Invoke-RestMethod -Uri "http://127.0.0.1:$ReceiverPort/status" -TimeoutSec 3
    $expectedDb = [IO.Path]::GetFullPath($TestDb)
    Assert-Evidence ([int]$ReceiverStatus.pid -eq $ReceiverProcess.Id) "Receiver /status PID" "PID $($ReceiverStatus.pid)"
    Assert-Evidence ([IO.Path]::GetFullPath([string]$ReceiverStatus.database_path) -eq $expectedDb) "Receiver /status database" $ReceiverStatus.database_path
    $Manifest.expected_extension_id = [string]$ReceiverStatus.expected_extension_id

    $BrowserExe = Get-BrowserExecutable
    $BrowserProcessName = [IO.Path]::GetFileName($BrowserExe)
    $Manifest.browser_executable = $BrowserExe
    $browserArgs = @(
        "--user-data-dir=$ProfileDir",
        "--remote-debugging-port=0",
        "--no-first-run",
        "--no-default-browser-check",
        "--disable-background-networking",
        "--disable-component-update",
        "--disable-default-apps",
        "--disable-sync",
        "--disable-extensions-except=$ExtensionDir",
        "--load-extension=$ExtensionDir",
        "--window-size=1280,800",
        "about:blank"
    )
    $BrowserRootProcess = Start-ToolProcess $BrowserExe $browserArgs $RepoRoot
    $Manifest.browser_root_pid = $BrowserRootProcess.Id

    $devtoolsLines = Wait-FileLines (Join-Path $ProfileDir "DevToolsActivePort") 2 25
    $BrowserDebugPort = [int]$devtoolsLines[0]
    $BrowserWsPath = [string]$devtoolsLines[1]
    $Manifest.browser_debug_port = $BrowserDebugPort
    $BrowserSocket = Connect-Cdp "ws://127.0.0.1:$BrowserDebugPort$BrowserWsPath"
    Add-Result "Dedicated browser profile" "PASS" $ProfileDir

    $extensionTarget = Find-ExtensionTarget $BrowserSocket 30
    if ($null -eq $extensionTarget) {
        Add-Result "Dynamic extension discovery" "EVIDENCE_INSUFFICIENT" "No extension service worker target appeared"
        throw "Could not dynamically discover the unpacked extension ID"
    }
    $ExtensionId = [string]$extensionTarget.extensionId
    $Manifest.extension_id = $ExtensionId
    Assert-Evidence (-not [string]::IsNullOrWhiteSpace($ExtensionId)) "Dynamic extension discovery" $ExtensionId
    Assert-Evidence ($ExtensionId -eq [string]$ReceiverStatus.expected_extension_id) "Extension identity matches Receiver policy" $ExtensionId

    $attach = Send-Cdp $BrowserSocket "Target.attachToTarget" @{ targetId = $extensionTarget.target.targetId; flatten = $true }
    $ServiceSession = [string]$attach.sessionId
    $previous = Evaluate-Cdp $BrowserSocket $ServiceSession "new Promise(resolve => chrome.storage.local.get({diagnosticsEnabled:false, receiverPort:38765}).then(resolve))"
    $PreviousDiagnosticsEnabled = [bool]$previous.diagnosticsEnabled
    $setExpression = "new Promise((resolve,reject)=>chrome.storage.local.set({diagnosticsEnabled:true,receiverPort:$ReceiverPort}).then(()=>resolve(true)).catch(e=>reject(String(e))))"
    [void](Evaluate-Cdp $BrowserSocket $ServiceSession $setExpression)
    Add-Result "Extension diagnostics enabled" "PASS" "receiverPort=$ReceiverPort"

    [void](Send-Cdp $BrowserSocket "Browser.setDownloadBehavior" @{ behavior = "allow"; downloadPath = $DownloadDir; eventsEnabled = $true })
    $mainTarget = Send-Cdp $BrowserSocket "Target.createTarget" @{ url = [string]$Fixture.url }
    $OwnedTargets.Add([string]$mainTarget.targetId) | Out-Null
    Start-Sleep -Seconds 6

    $downloadUrl = "http://127.0.0.1:$($Fixture.port)/download.bin"
    try {
        $downloadTarget = Send-Cdp $BrowserSocket "Target.createTarget" @{ url = $downloadUrl }
        if ($downloadTarget.targetId) { $OwnedTargets.Add([string]$downloadTarget.targetId) | Out-Null }
    } catch {
        # Navigating directly to a Content-Disposition attachment can close the
        # temporary target immediately. The downloads API evidence below is authoritative.
    }

    $deadline = [DateTime]::UtcNow.AddSeconds(35)
    do {
        Start-Sleep -Milliseconds 500
        if (Test-Path $TestDb) {
            try { $DatabaseEvidence = Query-TestDatabase $Python $TestDb } catch { $DatabaseEvidence = $null }
        }
        $haveRequests = $null -ne $DatabaseEvidence -and [int]$DatabaseEvidence.request_count -gt 0
        $havePositive = $null -ne $DatabaseEvidence -and [int]$DatabaseEvidence.positive_transferred_count -gt 0
        $haveDownload = $null -ne $DatabaseEvidence -and [int]$DatabaseEvidence.download_count -gt 0
    } while ((-not ($haveRequests -and $havePositive -and $haveDownload)) -and [DateTime]::UtcNow -lt $deadline)

    Assert-Evidence $haveRequests "Browser request persisted" "request_count=$($DatabaseEvidence.request_count)"
    Assert-Evidence $havePositive "Actual transferred bytes persisted" "positive_transferred_count=$($DatabaseEvidence.positive_transferred_count)"
    Assert-Evidence $haveDownload "Browser download persisted" "download_count=$($DatabaseEvidence.download_count)"

    $extensionHealth = Evaluate-Cdp $BrowserSocket $ServiceSession "({enabled:diagnosticsEnabled,attachedTabs:attachedTabs.size,queueLength:eventQueue.length,lastAttachError,lastReceiverError})"
    Assert-Evidence ([bool]$extensionHealth.enabled) "Extension runtime enabled" ($extensionHealth | ConvertTo-Json -Compress)
    Assert-Evidence ([int]$extensionHealth.attachedTabs -gt 0) "At least one browser tab attached" "attachedTabs=$($extensionHealth.attachedTabs)"
    Assert-Evidence ([string]::IsNullOrWhiteSpace([string]$extensionHealth.lastReceiverError)) "Extension Receiver delivery healthy" "queueLength=$($extensionHealth.queueLength)"

    $beforeRestartCount = [int]$DatabaseEvidence.request_count
    Stop-ExactProcess $ReceiverProcess "Receiver"
    $ReceiverProcess = $null
    $stoppedProbeFailed = $false
    try {
        Invoke-RestMethod -Uri "http://127.0.0.1:$ReceiverPort/status" -TimeoutSec 1 | Out-Null
    } catch { $stoppedProbeFailed = $true }
    Assert-Evidence $stoppedProbeFailed "Receiver actually stopped" "port $ReceiverPort no longer answered /status"

    # Do not start a replacement receiver unless the owned old process is
    # proven stopped. This prevents the false-positive multi-GUI scenario.
    $ReceiverProcess = Start-ToolProcess $Cli @("browser-receiver", "--db", $TestDb, "--port", "$ReceiverPort") $RepoRoot -RedirectOutput
    $restartLine = $ReceiverProcess.StandardOutput.ReadLine()
    if ([string]::IsNullOrWhiteSpace($restartLine)) { throw "Restarted Receiver did not emit startup status" }
    $restartFields = @{}
    foreach ($field in ($restartLine -split "`t")) {
        $parts = $field.Split('=', 2)
        if ($parts.Count -eq 2) { $restartFields[$parts[0]] = $parts[1] }
    }
    $Manifest.receiver_pid_restart = $ReceiverProcess.Id
    $restartStatus = Invoke-RestMethod -Uri "http://127.0.0.1:$ReceiverPort/status" -TimeoutSec 3
    Assert-Evidence ([int]$restartStatus.pid -eq $ReceiverProcess.Id) "Receiver clean restart PID" "PID $($ReceiverProcess.Id)"
    Assert-Evidence ([IO.Path]::GetFullPath([string]$restartStatus.database_path) -eq $expectedDb) "Receiver clean restart database" $restartStatus.database_path

    $postRestartTarget = Send-Cdp $BrowserSocket "Target.createTarget" @{ url = "http://127.0.0.1:$($Fixture.port)/index.html?restart=1" }
    $OwnedTargets.Add([string]$postRestartTarget.targetId) | Out-Null
    $deadline = [DateTime]::UtcNow.AddSeconds(25)
    do {
        Start-Sleep -Milliseconds 500
        $DatabaseEvidence = Query-TestDatabase $Python $TestDb
    } while ([int]$DatabaseEvidence.request_count -le $beforeRestartCount -and [DateTime]::UtcNow -lt $deadline)
    Assert-Evidence ([int]$DatabaseEvidence.request_count -gt $beforeRestartCount) "Post-restart browser event persisted" "before=$beforeRestartCount after=$($DatabaseEvidence.request_count)"

    $statusAfterRestart = Invoke-RestMethod -Uri "http://127.0.0.1:$ReceiverPort/status" -TimeoutSec 3
    Assert-Evidence ([int64]$statusAfterRestart.accepted_events -gt 0) "Receiver post-restart event count" "accepted_events=$($statusAfterRestart.accepted_events)"

    $ownedBrowser = @(Get-OwnedBrowserProcesses $ProfileDir $BrowserProcessName)
    $Manifest.browser_owned_pids = @($ownedBrowser.ProcessId)
    Assert-Evidence ($ownedBrowser.Count -gt 0) "Browser process ownership manifest" ($ownedBrowser.ProcessId -join ",")
} catch {
    $FatalMessage = $_.Exception.Message
    if (-not ($Results | Where-Object { $_.status -eq "FAIL" -or $_.status -eq "EVIDENCE_INSUFFICIENT" })) {
        Add-Result "Unhandled validation step" "FAIL" $FatalMessage
    }
} finally {
    # Restore the diagnostics flag in the dedicated profile before teardown.
    if ($null -ne $BrowserSocket -and -not [string]::IsNullOrWhiteSpace($ServiceSession)) {
        try {
            $restoreValue = if ($PreviousDiagnosticsEnabled) { "true" } else { "false" }
            [void](Evaluate-Cdp $BrowserSocket $ServiceSession "chrome.storage.local.set({diagnosticsEnabled:$restoreValue}).then(()=>true)")
            Add-Result "Extension diagnostics setting restored" "PASS" "diagnosticsEnabled=$restoreValue"
        } catch {
            Add-Result "Extension diagnostics setting restored" "FAIL" $_.Exception.Message
            if (-not $FatalMessage) { $FatalMessage = $_.Exception.Message }
        }
    } else {
        Add-Result "Extension diagnostics setting restored" "NOT_RUN" "Extension session was not established"
    }

    if ($null -ne $BrowserSocket) {
        foreach ($targetId in $OwnedTargets) {
            try { [void](Send-Cdp $BrowserSocket "Target.closeTarget" @{ targetId = $targetId }) } catch { }
        }
        try { $BrowserSocket.Dispose() } catch { }
    }

    if ($BrowserExe -and $BrowserProcessName) {
        try {
            Stop-OwnedBrowserProcesses $ProfileDir $BrowserProcessName
            Add-Result "Owned browser cleanup" "PASS" "Only dedicated-profile processes were stopped"
        } catch {
            Add-Result "Owned browser cleanup" "FAIL" $_.Exception.Message
            if (-not $FatalMessage) { $FatalMessage = $_.Exception.Message }
        }
    }

    try {
        Stop-ExactProcess $ReceiverProcess "Receiver"
        Add-Result "Receiver cleanup" "PASS" "Owned Receiver process stopped"
    } catch {
        Add-Result "Receiver cleanup" "FAIL" $_.Exception.Message
        if (-not $FatalMessage) { $FatalMessage = $_.Exception.Message }
    }
    try {
        Stop-ExactProcess $FixtureProcess "fixture"
        Add-Result "Fixture cleanup" "PASS" "Owned fixture process stopped"
    } catch {
        Add-Result "Fixture cleanup" "FAIL" $_.Exception.Message
        if (-not $FatalMessage) { $FatalMessage = $_.Exception.Message }
    }

    $hasFailure = @($Results | Where-Object { $_.status -eq "FAIL" -or $_.status -eq "EVIDENCE_INSUFFICIENT" }).Count -gt 0
    $Overall = if ($hasFailure -or $FatalMessage) { "FAIL" } else { "PASS" }
    Write-Reports $Overall $Manifest $DatabaseEvidence
}

if ($Overall -ne "PASS") {
    Write-Error "Windows browser diagnostics E2E failed: $FatalMessage"
    exit 1
}
Write-Host "PASS - isolated browser diagnostics E2E completed without touching the user profile or database."
exit 0
