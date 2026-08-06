# Example TLS Fixture

## File Properties

- **Path**: `tests/fixtures/example_tls.pcap`
- **Format**: Classic pcap, little-endian, microsecond timestamps
- **Linktype**: 1 (Ethernet)
- **Size**: 316 bytes
- **SHA-256**: `5308e89d7213d08c8dfa834fe9005060242050f27f672640b3cdb402b1ce57f6`

## Packet Table

| # | Unix timestamp | Direction/Protocol | Wire len | Expected attribution |
|---:|---:|---|---:|---|
| 1 | 1700000000 | TCP client->server, ClientHello `example.com` | 126 | example.com |
| 2 | 1700000001 | TCP server->client, TLS application data | 64 | example.com |
| 3 | 1700000002 | UDP client->server, dst 443 | 54 | (unknown) |

## Expected Query Results

After `run_offline` with this fixture:

- `example.com`: bytes=190, packets=2
- `(unknown)`: bytes=54, packets=1

## Rebuild Command (Windows)

```powershell
$b64='1MOyoQIABAAAAAAAAAAAAP//AAABAAAAAPFTZQAAAAB+AAAAfgAAAAARIjNEVWZ3iJmquwgARQAAcBI0QABABjBvwAACCl242CLMdQG7AAAAAQAAAABQGPrwAAAAABYDAQBDAQAAPwMDAAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8AAAITAQEAABQAAAAQAA4AAAtleGFtcGxlLmNvbQHxU2UAAAAAQAAAAEAAAABmd4iZqrsAESIzRFUIAEUAADISNUAAQAYwrF242CLAAAIKAbvMdQAAAGQAAABJUBj68AAAAAAXAwMABQECAwQFAvFTZQAAAAA2AAAANgAAAAARIjNEVWZ3iJmquwgARQAAKBI2QABAEWSDwAACCgEBAQHPCAG7ABQAAHF1aWMtcGF5bG9hZA=='
New-Item -ItemType Directory -Force tests\fixtures | Out-Null
[IO.File]::WriteAllBytes('tests\fixtures\example_tls.pcap', [Convert]::FromBase64String($b64))
(Get-FileHash tests\fixtures\example_tls.pcap -Algorithm SHA256).Hash.ToLower()
```

The last command must output `5308e89d7213d08c8dfa834fe9005060242050f27f672640b3cdb402b1ce57f6`.
