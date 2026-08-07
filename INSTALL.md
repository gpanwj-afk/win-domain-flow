# win-domain-flow 0.6 Installation

The current Windows installation, upgrade, browser diagnostics, isolation, and source-build instructions are maintained in:

- [`INSTALL_CN.md`](INSTALL_CN.md), complete Chinese Windows guide
- [`browser-extension/README_CN.md`](browser-extension/README_CN.md), Edge/Chrome diagnostics guide
- [`README.md`](README.md), product overview, reliability model, and privacy boundaries

Quick start:

1. Install official Npcap on Windows 10/11 x64.
2. Extract the Windows x64 package.
3. Double-click `Start-GUI-As-Administrator.cmd`.
4. Confirm the absolute SQLite path shown by the GUI, select the active physical adapter, and start capture.
5. For request/download diagnostics, load the unpacked `browser-extension` folder in Edge or Chrome and explicitly enable deep diagnostics.

v0.6 adds a stable packaged extension identity, exact Receiver Origin authentication, a persisted bounded retry queue, monotonic transferred-byte updates, redirect preservation, `/status` PID/database observability, duplicate-GUI protection, and explicit legacy-database migration using SQLite's online backup API.

The optional extension only has loopback host permission (`http://127.0.0.1/*`) and does not read HTTPS response bodies.

## Isolated validation

The package includes:

```text
tools\validate-windows.ps1
tools\browser_fixture.py
tools\query_e2e_db.py
```

The validator creates a temporary browser profile, temporary SQLite database, dynamic CDP/Receiver ports, and a local-only fixture. It does not operate on the user's default browser profile or production database. It dynamically discovers the unpacked extension ID, verifies Receiver PID/database ownership via `/status`, observes SQLite through read-only queries, verifies queued-event replay across a Receiver outage, and emits JSON/JUnit evidence plus a process/profile manifest.
