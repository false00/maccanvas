<img src="assets/logo.svg" align="right" width="72" alt="MacCanvas screen-and-canvas logo">

# MacCanvas

A lightweight Windows viewer for macOS Screen Sharing.
One portable viewer. No extra DLLs or subscription.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/connections-dark.png">
  <img alt="MacCanvas showing saved Macs and the connection workspace" src="docs/images/connections-light.png">
</picture>

*The app follows the Windows theme. Screenshot uses demonstration machines.*

## Features

- **Sharp scaling:** fit a 4K desktop to your screen without changing the Mac's resolution.
- **Visible cursor:** keep your normal Windows pointer and accessibility size.
- **Easy connections:** searchable Mac list, quick connect, and an animated, collapsible sidebar.
- **Protected logins:** Windows Credential Manager storage; **Windows Hello is mandatory for every saved login**. Saved passwords cannot be displayed.
- **Live statistics:** animated download/upload history, automatic kbps/Mbps units, and an optional FPS counter.
- **HEVC performance modes:** Apple's H.265 stream at up to 30 or 60 FPS, with encrypted control/video and a bundled native decoder.
- **Mac audio:** optional HEVC-session playback through your Windows output, with local volume and mute controls.
- **Quality controls:** lossless VNC and JPEG options, maximum update rate, and a VNC bandwidth target, with an in-app comparison guide.
- **Familiar controls:** mouse, keyboard, Windows-sensitive scrolling, full screen, and optional Ctrl-to-Command shortcuts.
- **Glass-inspired appearance:** black/charcoal in dark mode, Apple-style whites and grays in light mode, and system-blue controls. Follows Windows appearance preferences.
- **Easy updates:** checks the official release on launch and prompts before a verified, one-click update and restart.
- **Clipboard and files:** opt-in VNC text sharing, encrypted Unicode clipboard, and verified SFTP file transfers.
- **Local security audit:** structured JSONL evidence, process ancestry, connection history, and file-transfer receipts.

## Install

**Windows 10/11 x64**, PowerShell 5.1+. Run as your normal user:

```powershell
irm https://raw.githubusercontent.com/false00/maccanvas/main/install.ps1 | iex
```

Run the same command to update. The installer verifies SHA-256 digests and creates
Desktop and Start-menu shortcuts. Installed copies repair missing or outdated shortcuts
on launch, including after an in-app update. Use `-NoShortcut` to skip both.
Prefer portable? Download `MacCanvas-windows-x64.exe`
from [Releases](https://github.com/false00/maccanvas/releases/latest) and run it.

When a newer version is available, choose **Update & restart** or **Later**.
Disconnect first if a Mac session is active. The app verifies the download and
updates its own executable without changing saved Macs or credentials.
Its folder must be writable. **Check updates** in the sidebar retries the check;
the installer command remains available for manual updates.

**Unsigned application:** only run downloads and remote scripts you trust.
[Read the installer](install.ps1) first. Checksums do not establish publisher trust
or bypass Windows application-control policies.

## Connect

1. On the Mac, enable **System Settings → General → Sharing → Screen Sharing**
   and allow your macOS account.
2. Select **New connection**. Enter the Mac's name, IP/VPN address, and port `5900`.
3. Choose **Mac account** and enter its username/password. Select **Connect to Mac**.
4. To reconnect with Hello, enable **Save login in Credential Manager**.
   Credentials are saved only after a successful login.

**VNC password** is for the separate password configured in Screen Sharing,
not the Mac account password; select a lossless VNC mode in **Quality** to use it.
If necessary, use your macOS short username.

Use a VPN for internet access. **Never expose port 5900 publicly.** Standard
Mac VNC screen and input traffic is not encrypted, even with Mac account login.

## Quality and statistics

Open **Quality** before or during a connection. The default is **HEVC / H.265
at up to 60 FPS**, with adaptive bitrate. Existing saved choices are preserved.

Methods are listed in recommended order: **HEVC 60, HEVC 30, Auto, ZRLE, Tight,
Tight + JPEG, Hextile, Zlib, Raw, RRE, CoRRE**, with CopyRect available for VNC.
The in-app guide explains CPU cost, image quality, and illustrative
bandwidth ranges. The Mac can choose a fallback or ignore compression hints.
Choose **30 FPS** for lower decoding load or **60 FPS** for smoother motion.
HEVC uses **Mac account** login and needs a compatible Apple-silicon Mac
running macOS 14 or later and UDP access on the VNC port and the next port
(normally `5900` and `5901`). It shares the physical desktop without changing
the Mac's resolution or installing a Mac helper. Native CPU decoding is bundled;
no Windows codec extension is required.

The HEVC presets request different frame-rate ceilings, not fixed bitrates.
Actual FPS depends on the Mac, network, and Windows decoding capacity. The VNC
compression/JPEG and request-bandwidth controls do not apply to HEVC.

### Client graphics support

| Client GPU | HEVC 4:4:4 decoding | Requirements and verification |
| --- | --- | --- |
| NVIDIA discrete/laptop GPU | NVDEC preferred; D3D11 fallback | Compatible 8-bit 4:4:4 driver; verified on an RTX 4090 Laptop GPU. No CUDA toolkit required. |
| Intel integrated or discrete GPU | D3D11 through Windows Media Foundation | Compatible 4:4:4 driver and the Windows HEVC decoder component; real 4K pixels and recovery verified on Intel UHD Graphics. |
| AMD integrated or discrete GPU | The same vendor-neutral D3D11 path | Compatible 4:4:4 driver and the Windows HEVC decoder component. Selection/fallback is tested; physical AMD hardware has not been tested for this release. |
| Missing/unsupported hardware decoder | Bundled libde265 CPU fallback | No Windows codec component is required for software decoding; GPU composition/scaling remains available where supported. |

Integrated-only systems are supported; a discrete GPU is not required. Hardware
decoding is selected only when the adapter supports the stream format, and D3D11
output must remain decoder-bound AYUV 4:4:4 rather than silently dropping chroma.
HEVC60 is a ceiling, not a 60 FPS guarantee: older GPUs/CPUs, 4K, and slower links
may benefit from the HEVC30 preset. Run `--video-info` to inspect the installed
adapters and decoder components; actual stream negotiation can still fall back.

**Apply** reconnects the current Mac automatically when switching transports or
HEVC presets; saved logins require fresh Windows Hello approval. Other quality
changes apply live. H.264 playback is not supported.

A bandwidth target paces screen requests; **it is not a guaranteed bitrate cap**.
Bursts can exceed it. Keyboard and mouse input are not throttled.
Traffic measures VNC, HEVC/audio SRTP, and SSH payloads, excluding TCP/UDP/VPN overhead and update checks. FPS counts unique desktop
images submitted for display, not monitor refreshes; idle screens can read 0 FPS.

## Audio

Open the **speaker button** and choose **Enable audio**. The app downloads and
verifies a small, separately licensed AAC converter from the matching official
release. Audio works with compatible **HEVC / H.265** sessions; ordinary VNC
does not carry sound. Enabling or retrying sound reconnects an active HEVC
session automatically, with fresh Hello approval for saved logins.

Playback follows your Windows output device. Volume and mute are local to
MacCanvas; they do not change the Mac's volume. No microphone audio is captured
or sent. Audio content is never written to logs or media files.

The optional converter uses standard LOAS/WAVE pipes, not a linked viewer codec.
[Source and separate license terms](crates/audio-codec-tool/README.md).

## Clipboard and files

Open **Clipboard** to enable VNC plain-text sharing. Ctrl+V sends Windows text
when pasting into the Mac; incoming Mac text can replace your Windows clipboard.
Standard VNC supports Latin-1; Unicode requires a negotiated UTF-8 extension.
HEVC sessions use the encrypted SSH clipboard instead of VNC clipboard sharing.

For encrypted Unicode copy/paste, use **Copy Mac text** and **Send Windows text**
in **Clipboard**. Use **Files** for file transfers. Both panels share one SSH
connection, including the same login and host-key verification. Closing a panel
keeps the established SSH connection open.

Enable **Remote Login** in the Mac's Sharing settings. Verify the SSH fingerprint
before trusting a new Mac. Saved Mac logins require fresh Windows Hello approval.

Send or receive single files without overwriting existing files. Uploads are
SHA-256 checked by reading them back before the final file is created.
Folders, images/rich-text clipboard formats, and Apple's proprietary drag-and-drop
transfer are not supported. [Setup and compatibility](docs/sharing.md).

## Saved-login privacy

Passwords never enter profile files or a saved-password display field.
Every [saved-login attempt](docs/images/saved-login.png), including SSH, requires a fresh Hello approval.
If Hello is unavailable or cancelled, the app neither reads nor sends the saved password.
SSH verifies the host key before requesting Hello.
You may instead enter credentials manually; this does not unlock the saved login.
Unsaved credentials stay in zeroizing process memory only while connected, so
stream-mode changes can reconnect without saving them. Disconnecting clears them.

Delete a Mac in the app to remove its saved credential. Saved logins are tied to
their original address. Hello is an app approval gate, not extra vault encryption:
other software running as your Windows user may access generic Windows credentials.

## Security logging

`~\.maccanvas\log.json` is **JSONL: one JSON record per line**.
Open its folder from **About**.

Local evidence includes executable hashes, build details, Windows identity, parent-process
ancestry, network endpoints, Hello/authentication outcomes, updates, and file-transfer
paths, sizes, and SHA-256 receipts. Logs have per-user Windows permissions and bounded rotation.
Passwords, PINs, keystrokes, clipboard contents, and file contents are never logged.

Logs contain sensitive metadata and are **not encrypted or tamper-proof**.
[Logging, retention, and integrity verification](docs/security-logging.md).

## Remove

Close MacCanvas, then:

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/false00/maccanvas/main/install.ps1))) -Uninstall
```

The managed application and its Desktop and Start-menu shortcuts are removed. Profiles, credentials, downloaded audio components, and audit logs are
preserved; delete saved Macs in the app first to remove their logins.
For a portable copy, delete the executable.

## Build and credits

Install rustup, Visual Studio C++ Build Tools, and CMake 3.15+, then:

```powershell
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
cargo build --release --locked
```

The pinned toolchain builds `target\x86_64-pc-windows-msvc\release\maccanvas.exe`.
Rust + Slint with GPU rendering, automatic NVIDIA NVDEC or cross-vendor D3D11
decoding for supported 8-bit HEVC 4:4:4 streams, a bundled libde265 software
fallback, and static Microsoft C runtime. D3D11 decoding uses the installed
Windows HEVC decoder component; NVIDIA NVDEC does not need it or a CUDA toolkit.
Windows releases are built and tested in GitHub Actions.

Inspect adapter and decoder capabilities without connecting:

```powershell
cargo run --release -- --video-info
```

For fallback testing, set `$env:MACCANVAS_VIDEO_DECODER = 'software'` before
launching; remove it with `Remove-Item Env:MACCANVAS_VIDEO_DECODER` to return to
automatic selection. `SLINT_BACKEND=winit-software` selects software UI rendering.
Use `MACCANVAS_VIDEO_DECODER=d3d11` to test the Intel/AMD-first D3D11 path on a
hybrid system, or `MACCANVAS_VIDEO_DECODER=nvdec` to test NVIDIA NVDEC only.
GPU decoding and color conversion use NVDEC or D3D11, followed by a readback for
the Slint GPU renderer. Direct D3D11/OpenGL texture sharing is experimental and
disabled by default after failing live presentation testing; it can be requested
with `MACCANVAS_GPU_PRESENTATION=shared` for diagnostics only. Unsupported streams
or hardware decode errors fall back to libde265. HEVC frames use a bounded
latest-frame pacer, and cached Mac cursor shapes are drawn locally.

Protocol references and prior art: [TigerVNC](https://tigervnc.org/),
RealVNC, TightVNC, the original VNC authors, and
[iShareScreen](https://github.com/renegadelink/iShareScreen) for Apple's
High Performance protocol implementation. Low-latency client rendering patterns
were informed by [Moonlight](https://github.com/moonlight-stream/moonlight-qt).

**Viewer: AGPL-3.0-or-later.** The optional audio converter has separate
[0BSD/Fraunhofer terms](crates/audio-codec-tool/README.md).
Dependency credits: [THIRD_PARTY_NOTICES.txt](THIRD_PARTY_NOTICES.txt)
or `--licenses`.
