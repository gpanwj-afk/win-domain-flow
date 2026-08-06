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
- **Npcap** installed (download from https://npcap.com/)
- **Rust** toolchain (MSVC): `rustup default stable-x86_64-pc-windows-msvc`
- **Visual Studio Build Tools 2022** with C++ workload

## Building

```powershell
# Set Npcap SDK paths
$env:LIB="C:\Npcap-SDK\Lib\x64;$env:LIB"
$env:INCLUDE="C:\Npcap-SDK\Include;$env:INCLUDE"

# Build
cargo build --release
```

## Usage

### List Capture Devices

```powershell
.\target\release\win-domain-flow.exe devices
```

Output format (TSV):
```
name	description
\Device\NPF_{...}	Network adapter description
```

### Live Capture

```powershell
.\target\release\win-domain-flow.exe capture --interface "\Device\NPF_{...}"
```

Options:
- `--interface`: Capture interface name (required)
- `--db`: Database path (default: `domainflow.db`)
- `--flush-seconds`: Flush interval 1-60 (default: 1)
- `--idle-seconds`: Flow idle timeout 1-86400 (default: 300)
- `--bpf`: BPF filter (default: `tcp port 443 or udp port 443`)

### Query Top Domains

```powershell
.\target\release\win-domain-flow.exe top
```

Options:
- `--db`: Database path (default: `domainflow.db`)
- `--days`: Look back N days (default: 1)
- `--limit`: Maximum rows (default: 20)

Output format (TSV):
```
domain	bytes	packets
example.com	123456	789
```

## Precision Boundaries

### SNI Extraction Limitations

- **Plaintext TLS ClientHello only**: ECH (Encrypted Client Hello) will not be decoded
- **Connection reuse**: Sessions that resume or multiplex may show as `(unknown)`
- **Truncated captures**: Partial ClientHello may not yield SNI
- **Out-of-order packets**: TCP segments arriving out of order may delay SNI extraction
- **Pre-existing connections**: Flows established before capture starts may not have SNI

### UDP/443

All UDP/443 traffic is attributed to `(unknown)` since there is no plaintext SNI in UDP-based TLS (QUIC).

### Wire Bytes

Reported bytes include all protocol headers (IP, TCP/UDP, Ethernet) and may include retransmissions.

### Database Mergins

Multiple capture sessions using the same database file will have their counts merged. Different network interfaces writing to the same DB will also merge.

## Testing

### Unit Tests

```powershell
cargo test
```

### Integration Tests

Requires the pcap fixture at `tests/fixtures/example_tls.pcap` (SHA-256 verified).

```powershell
cargo test --test offline_pipeline
```

### Clippy

```powershell
cargo clippy --all-targets -- -D warnings
```

### Format Check

```powershell
cargo fmt --check
```

## Manual Capture

For manual testing with Wireshark's dumpcap:

```bash
# List interfaces
dumpcap.exe -D

# Capture 20 seconds of TLS traffic
dumpcap.exe -i 1 -f "tcp port 443 or udp port 443" -a duration:20 -F pcap -s 0 -w tests\fixtures\manual_tls.pcap
```

## License

This project does not commit to a specific license. See repository for details.
