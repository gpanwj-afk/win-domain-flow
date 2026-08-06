# win-domain-flow

Lightweight Windows domain-level network byte monitor. Captures TLS/HTTPS traffic via Npcap, extracts SNI from TLS ClientHello, and aggregates per-domain daily byte/packet counts in SQLite.

## Project Scope

This tool provides:

- Network packet capture via Npcap (libpcap)
- TLS ClientHello SNI extraction (plaintext only)
- Per-domain daily byte and packet counting
- SQLite-based persistent storage with WAL mode
- Simple CLI for live capture, device listing, and top domains query

### What This Tool Does NOT Do

- DNS-based domain resolution
- QUIC SNI extraction
- Process attribution (v2 feature, currently disabled)
- TLS certificate inspection
- Real-time TUI or dashboard
- Traffic filtering by process

## Prerequisites

- **Windows 10/11** (x64)
- **Npcap** installed (WinPcap API-compatible Mode)
- **Rust 1.88.0** MSVC toolchain
- **Visual Studio Build Tools 2022** with the C++ workload

> **⚠️ Important:** Do NOT use Win10Pcap as a replacement. It has driver compatibility issues on many Windows versions. Always install the official Npcap from https://npcap.com with "WinPcap API-compatible Mode" enabled.

For detailed installation instructions and troubleshooting, see [INSTALL.md](INSTALL.md) ([中文版](INSTALL_CN.md)).

## Building

```powershell
# Set Npcap SDK paths when they are not already configured system-wide.
$env:LIB="C:\Npcap-SDK\Lib\x64;$env:LIB"
$env:INCLUDE="C:\Npcap-SDK\Include;$env:INCLUDE"

cargo build --release
```

## Usage

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
- **Connection reuse**: sessions that resume or multiplex may remain attributed to the SNI observed at connection establishment.
- **Truncated captures**: a ClientHello that exceeds the capture boundary may not yield SNI.
- **Out-of-order or missing TCP segments**: a sequence gap ends best-effort ClientHello inspection for that connection; its unresolved bytes are recorded as `(unknown)`.
- **Pre-existing connections**: flows established before capture starts may not expose a ClientHello and may remain `(unknown)`.

### UDP/443

Every UDP/443 packet is attributed immediately to `(unknown)`. TCP and UDP keys include the transport protocol, so an identical endpoint tuple cannot inherit attribution across protocols.

### Wire Bytes

Reported bytes use the pcap packet header wire length and include link, network, and transport headers. Retransmitted packets are counted because they consumed observed network capacity, while retransmitted TLS payload is not appended twice to the ClientHello parser.

### Database Merging

Multiple capture sessions using the same database file merge their counts through additive SQLite upserts. Different interfaces writing to the same database also merge because the MVP schema intentionally has no interface dimension.

## Testing

```powershell
cargo fmt --check
cargo check --all-targets
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo build --release
```

The integration fixture is `tests/fixtures/example_tls.pcap`. Its expected SHA-256 is documented in `tests/fixtures/README.md`.

## Manual Capture

For manual testing with Wireshark's `dumpcap`:

```powershell
dumpcap.exe -D
dumpcap.exe -i 1 -f "tcp port 443 or udp port 443" -a duration:20 -F pcap -s 0 -w tests\fixtures\manual_tls.pcap
```

Run the application in an elevated terminal when Npcap permissions require it. Generate ordinary HTTPS traffic during capture, stop with Ctrl+C, then query the resulting database with the `top` command.

## License

MIT. See `Cargo.toml` for the package license declaration.
