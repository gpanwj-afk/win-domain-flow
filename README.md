# win-domain-flow

Lightweight Windows domain-level network byte monitor. It captures TLS/HTTPS traffic through Npcap, extracts SNI from TLS ClientHello, and aggregates daily domain byte and packet counts in SQLite.

## Project Scope

This tool provides:

- Network packet capture through Npcap/libpcap
- TLS ClientHello SNI extraction for plaintext ClientHello records
- Per-domain daily byte and packet counting
- SQLite persistence with WAL mode
- A native desktop dashboard for capture control and traffic visualization
- A CLI for automation, device listing, capture, and top-domain queries

### What This Tool Does NOT Do

- DNS-based domain resolution
- QUIC SNI extraction
- Process attribution, currently reserved for a later version
- TLS certificate inspection
- Traffic filtering by process

## Prerequisites

- **Windows 10/11**, x64
- **Npcap** installed with WinPcap API-compatible Mode
- **Rust 1.88.0** MSVC toolchain
- **Visual Studio Build Tools 2022** with the C++ workload

> Do not use Win10Pcap as a replacement. Install official Npcap and enable WinPcap API-compatible Mode.

For detailed installation instructions and troubleshooting, see [INSTALL.md](INSTALL.md) or [INSTALL_CN.md](INSTALL_CN.md).

## Building

```powershell
# Set Npcap SDK paths when they are not configured system-wide.
$env:LIB="C:\Npcap-SDK\Lib\x64;$env:LIB"
$env:INCLUDE="C:\Npcap-SDK\Include;$env:INCLUDE"

cargo build --release --locked
```

The release build produces two programs:

- `target\release\win-domain-flow-gui.exe`: native desktop dashboard
- `target\release\win-domain-flow.exe`: command-line interface

## Desktop Dashboard

Run PowerShell as Administrator, then launch:

```powershell
.\target\release\win-domain-flow-gui.exe
```

The GUI can also be opened by double-clicking `win-domain-flow-gui.exe`. The Windows GUI executable does not open an additional console window.

The dashboard provides:

- Npcap adapter discovery and selection
- Start Capture and Stop and Flush controls
- Configurable SQLite database path
- Automatic or manual refresh
- Visible traffic bytes and packet totals
- Known-domain traffic share
- Database growth rate while capture is running
- Horizontal traffic bars for the leading domains
- A ranked domain, byte, and packet table
- Final capture summary after a graceful stop

Stopping capture from the GUI uses the same shutdown-aware runtime path as Ctrl+C. Pending flow bytes are drained and the SQLite writer is shut down before the capture worker exits.

Npcap live capture normally requires Administrator privileges. If capture fails immediately, restart the GUI from an elevated PowerShell or an elevated shortcut.

## Command-Line Usage

The original CLI remains available for scripts and advanced workflows.

### List Capture Devices

```powershell
.\target\release\win-domain-flow.exe devices
```

Output format:

```text
name\tdescription
\Device\NPF_{...}\tNetwork adapter description
```

### Live Capture

```powershell
.\target\release\win-domain-flow.exe capture --interface "\Device\NPF_{...}"
```

Options:

- `--interface`: capture interface name, required
- `--db`: database path, default `domainflow.db`
- `--flush-seconds`: flush interval from 1 to 60 seconds, default 1
- `--idle-seconds`: flow idle timeout from 1 to 86400 seconds, default 300
- `--bpf`: BPF filter, default `tcp port 443 or udp port 443`

### Query Top Domains

```powershell
.\target\release\win-domain-flow.exe top
```

Options:

- `--db`: database path, default `domainflow.db`
- `--days`: look back N UTC days, default 1
- `--limit`: maximum rows, default 20

Output format:

```text
domain\tbytes\tpackets
example.com\t123456\t789
```

## Precision Boundaries

### SNI Extraction Limitations

- **Plaintext TLS ClientHello only**: ECH cannot be decoded.
- **Connection reuse**: sessions that resume or multiplex remain attributed to the SNI observed at connection establishment.
- **Truncated captures**: a ClientHello beyond the capture boundary may not yield SNI.
- **Out-of-order or missing TCP segments**: a sequence gap ends best-effort ClientHello inspection for that connection; unresolved bytes are recorded as `(unknown)`.
- **Pre-existing connections**: flows established before capture starts may not expose a ClientHello and may remain `(unknown)`.

### UDP/443

Every UDP/443 packet is attributed immediately to `(unknown)`. TCP and UDP flow keys include the transport protocol, so an identical endpoint tuple cannot inherit attribution across protocols.

### Wire Bytes

Reported bytes use the pcap packet-header wire length and include link, network, and transport headers. Retransmitted packets are counted because they consumed observed capacity, while retransmitted TLS payload is not appended twice to the ClientHello parser.

### Dashboard Totals

The dashboard labels its summary as **visible traffic** because the totals are calculated from the currently displayed top rows. Increase the Rows setting when a wider aggregate view is needed.

### Database Merging

Multiple capture sessions using the same database file merge counts through additive SQLite upserts. Different interfaces writing to the same database also merge because the MVP schema intentionally has no interface dimension.

## Testing

```powershell
cargo fmt --check
cargo check --all-targets --locked
cargo test --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
cargo build --release --locked
```

The integration fixture is `tests/fixtures/example_tls.pcap`. Its expected SHA-256 is documented in `tests/fixtures/README.md`.

## Manual Capture

For manual testing with Wireshark's `dumpcap`:

```powershell
dumpcap.exe -D
dumpcap.exe -i 1 -f "tcp port 443 or udp port 443" -a duration:20 -F pcap -s 0 -w tests\fixtures\manual_tls.pcap
```

Run the application in an elevated session when Npcap permissions require it. Generate ordinary HTTPS traffic during capture, stop through the GUI or Ctrl+C, then verify the database through the dashboard or the CLI `top` command.

## License

MIT. See `Cargo.toml` for the package license declaration.
