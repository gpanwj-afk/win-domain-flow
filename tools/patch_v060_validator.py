from pathlib import Path

path = Path("tools/validate-windows.ps1")
text = path.read_text()
text = text.replace('throw "$Name: $Evidence"', 'throw "${Name}: $Evidence"', 1)
start = text.index("    $beforeRestartCount = [int]$DatabaseEvidence.request_count")
end = text.index("    $statusAfterRestart = Invoke-RestMethod", start)
replacement = r'''    $beforeRestartCount = [int]$DatabaseEvidence.request_count
    Stop-ExactProcess $ReceiverProcess "Receiver"
    $ReceiverProcess = $null
    $stoppedProbeFailed = $false
    try {
        Invoke-RestMethod -Uri "http://127.0.0.1:$ReceiverPort/status" -TimeoutSec 1 | Out-Null
    } catch { $stoppedProbeFailed = $true }
    Assert-Evidence $stoppedProbeFailed "Receiver actually stopped" "port $ReceiverPort no longer answered /status"

    # Generate real browser traffic while the Receiver is down. This must be
    # retained by the extension queue rather than written directly to SQLite.
    $outageTarget = Send-Cdp $BrowserSocket "Target.createTarget" @{ url = "http://127.0.0.1:$($Fixture.port)/index.html?receiver_outage=1" }
    $OwnedTargets.Add([string]$outageTarget.targetId) | Out-Null
    $deadline = [DateTime]::UtcNow.AddSeconds(20)
    $queuedDuringOutage = $false
    $receiverErrorDuringOutage = $false
    do {
        Start-Sleep -Milliseconds 400
        $outageHealth = Evaluate-Cdp $BrowserSocket $ServiceSession "({queueLength:eventQueue.length,lastReceiverError})"
        $queuedDuringOutage = [int]$outageHealth.queueLength -gt 0
        $receiverErrorDuringOutage = -not [string]::IsNullOrWhiteSpace([string]$outageHealth.lastReceiverError)
    } while ((-not ($queuedDuringOutage -and $receiverErrorDuringOutage)) -and [DateTime]::UtcNow -lt $deadline)
    Assert-Evidence $queuedDuringOutage "Receiver outage queues browser events" "queueLength=$($outageHealth.queueLength)"
    Assert-Evidence $receiverErrorDuringOutage "Receiver outage is visible to extension" ([string]$outageHealth.lastReceiverError)

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

    # The extension must recover by its own retry/alarm machinery. The validator
    # deliberately does not call processQueue() or synthesize /events requests.
    $deadline = [DateTime]::UtcNow.AddSeconds(40)
    $queueRecovered = $false
    $outageEventsPersisted = $false
    do {
        Start-Sleep -Milliseconds 500
        $recoveryHealth = Evaluate-Cdp $BrowserSocket $ServiceSession "({queueLength:eventQueue.length,lastReceiverError})"
        $DatabaseEvidence = Query-TestDatabase $Python $TestDb
        $queueRecovered = [int]$recoveryHealth.queueLength -eq 0 -and [string]::IsNullOrWhiteSpace([string]$recoveryHealth.lastReceiverError)
        $outageEventsPersisted = [int]$DatabaseEvidence.request_count -gt $beforeRestartCount
    } while ((-not ($queueRecovered -and $outageEventsPersisted)) -and [DateTime]::UtcNow -lt $deadline)
    Assert-Evidence $queueRecovered "Queued events replay after Receiver recovery" "queueLength=$($recoveryHealth.queueLength); error=$($recoveryHealth.lastReceiverError)"
    Assert-Evidence $outageEventsPersisted "Outage-period browser events persisted after recovery" "before=$beforeRestartCount after=$($DatabaseEvidence.request_count)"

    $afterReplayCount = [int]$DatabaseEvidence.request_count
    $postRestartTarget = Send-Cdp $BrowserSocket "Target.createTarget" @{ url = "http://127.0.0.1:$($Fixture.port)/index.html?restart=1" }
    $OwnedTargets.Add([string]$postRestartTarget.targetId) | Out-Null
    $deadline = [DateTime]::UtcNow.AddSeconds(25)
    do {
        Start-Sleep -Milliseconds 500
        $DatabaseEvidence = Query-TestDatabase $Python $TestDb
    } while ([int]$DatabaseEvidence.request_count -le $afterReplayCount -and [DateTime]::UtcNow -lt $deadline)
    Assert-Evidence ([int]$DatabaseEvidence.request_count -gt $afterReplayCount) "Post-restart new browser event persisted" "before=$afterReplayCount after=$($DatabaseEvidence.request_count)"

'''
text = text[:start] + replacement + text[end:]
path.write_text(text)
