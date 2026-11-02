# MacCanvas

**Your Mac. Your Windows screen.**

A small, portable Windows VNC viewer built for macOS Screen Sharing.
One executable. No server, browser, subscription, or extra runtime.

## What it does

- Connect with your **Mac username and password**, or a separately configured VNC password.
- Fit a 4K Mac desktop onto a smaller screen with **Lanczos scaling**, without changing the Mac's resolution.
- Keep your **normal Windows cursor**, including its accessibility size. It never shrinks with the remote desktop.
- Send mouse, keyboard, and scroll input. Windows **Ctrl shortcuts map to Command** by default.
- Save machine addresses locally. Store passwords **only in Windows Credential Manager**.
- Require **Windows Hello before reading a saved login**. You can also require Hello for every connection.
- Use a lightweight **Slint Fluent interface**, with light/dark themes and software rendering.

## Install

Windows 10/11 **x64**, PowerShell 5.1 or newer. Run as your normal user:

```powershell
irm https://raw.githubusercontent.com/false00/maccanvas/main/install.ps1 | iex
```

The installer checks release SHA-256 digests, verifies the x64 executable,
and creates a Start-menu shortcut. Updates use the same command.
Nothing requires administrator access.

**Prefer portable?** Download `MacCanvas-windows-x64.exe` from
[Releases](https://github.com/false00/maccanvas/releases/latest) and run it.
No DLL folder or installation is required.

**The executable is unsigned.** Only run it if you trust this repository.
[Read the installer](install.ps1) first. Checksums check the download, not the
publisher's trustworthiness. MacCanvas does not bypass SmartScreen or managed
Windows application-control policies.

## Connect to your Mac

1. On the Mac, open **System Settings → General → Sharing → Screen Sharing**.
2. Enable it and allow your macOS account access.
3. In MacCanvas, enter the Mac's IP address or VPN address. Leave port `5900`.
4. Choose **Mac account**, enter its username/password, and select **Connect to Mac**.
5. To remember it, select **Remember in Credential Manager** before connecting.
   The password is saved only after a successful login.

Use the Mac account's short username if its display name does not work.
**VNC password** mode is only for the separate password configured in the Mac's
Screen Sharing settings; it is not your macOS account password.

For internet access, use a VPN such as Tailscale. **Do not expose port 5900
directly to the internet.** macOS account authentication protects the login
exchange, but standard VNC screen, keyboard, and mouse traffic is **not encrypted**.
MacCanvas does not claim TLS protection or verify a Mac's identity.

## Saved logins and Windows Hello

Passwords are never written to profile files, logs, command lines, or environment
variables. Each machine has a separate Credential Manager entry. Select a saved
machine and approve the Windows Hello prompt to reconnect.

If Hello is unavailable or cancelled, **the saved password is not read and the
connection does not open**. You can uncheck **Use saved login** and enter credentials
manually, unless **Hello before every connection** is enabled.

Hello is an approval gate in this app, not an additional vault encryption boundary.
Other programs running as your Windows user may access that user's generic
Credential Manager entries. Use a protected Windows account and screen lock.

Deleting a machine in the app removes its saved credential. Changing an address
with a saved login requires a new machine entry, so a saved password cannot
accidentally be sent to a different endpoint.

## Controls

- Scaling adjusts automatically when you resize or enter full screen.
- Click inside the remote desktop before typing.
- **Ctrl+C / Ctrl+V / Ctrl+S** become the Mac's Command shortcuts by default.
  Turn off **Ctrl shortcuts act as Command** to send normal Control.
- **Alt** acts as Option. Windows/Meta acts as Command when delivered by Windows.
- System-reserved shortcuts such as Alt+Tab remain local.
- Clipboard synchronization, audio, file transfer, VNC hosting, and remote display
  resolution changes are intentionally not included.

## Remove

Close MacCanvas, then run:

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/false00/maccanvas/main/install.ps1))) -Uninstall
```

Only the managed executable, receipt, and matching Start-menu shortcut are removed.
Profiles and saved credentials are preserved. To remove a saved login, delete its
machine inside the app first. A portable copy can simply be deleted.

## Build

Install stable Rust **1.92+** and Visual Studio C++ Build Tools, then:

```powershell
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
cargo build --release --locked
```

The executable is `target\x86_64-pc-windows-msvc\release\maccanvas.exe`.
It statically links the Microsoft C runtime. Slint uses the software renderer;
Qt, WebView2, Skia, and external runtime DLLs are not required.

The release workflow tests the app and installer, builds the executable, checks
its Windows imports, and publishes versioned assets and `SHA256SUMS.txt`.

## Credits and license

MacCanvas is an independent Rust project inspired by
[TigerVNC](https://tigervnc.org/). Credit to the TigerVNC team, RealVNC,
TightVNC, and the original VNC authors for their work on remote desktop clients
and the RFB ecosystem. macOS Screen Sharing implements Apple's ARD authentication.

The implementation uses the RFB/ARD protocol documentation and Rust libraries;
it does not embed TigerVNC's C++ viewer. See
[THIRD_PARTY_NOTICES.txt](THIRD_PARTY_NOTICES.txt) and **`--licenses`** for
dependency acknowledgements.

Licensed under **GPL-3.0-or-later**, including compatibility with Slint's GPL option.
