# win-domain-flow 0.5 Installation

The current Windows installation, upgrade, browser diagnostics extension, troubleshooting, and source-build instructions are maintained in:

- [`INSTALL_CN.md`](INSTALL_CN.md), complete Chinese guide
- [`browser-extension/README_CN.md`](browser-extension/README_CN.md), Edge/Chrome diagnostics extension guide
- [`README.md`](README.md), product overview and capability boundaries

Quick start:

1. Install official Npcap on Windows 10/11 x64.
2. Extract the Windows x64 package.
3. Double-click `Start-GUI-As-Administrator.cmd`.
4. Select the active physical adapter and start capture.
5. For browser request and download diagnostics, load the unpacked `browser-extension` folder in Edge or Chrome, then explicitly enable deep diagnostics from the extension popup.

The optional extension sends metadata only to `127.0.0.1:38765`. It does not read HTTPS response bodies.
