# Clipboard and files

MacCanvas uses macOS Screen Sharing for the desktop and standard SSH/SFTP for
encrypted file transfer. No extra Mac application or Windows helper executable
is installed.

## VNC clipboard

HEVC sessions use the encrypted SSH clipboard described below. Automatic VNC
clipboard sharing is available in the standard VNC modes.

1. Connect to the Mac.
2. Open **Clipboard** and enable **Share plain-text clipboard over VNC**.
3. Use Ctrl+C / Ctrl+V with the default Ctrl-to-Command mapping.

Windows clipboard text is read only when you request a paste, not continuously
monitored. Incoming Mac text is applied while the remote canvas has focus and
the viewer is the foreground window. An incoming update can replace the Windows
clipboard. Pending Mac text is discarded if the Windows clipboard changes first.
Disabling sharing clears the viewer's cached clipboard text.

Standard RFB CutText uses ISO-8859-1 (Latin-1). MacCanvas never guesses that an
unnegotiated server uses UTF-8 and never replaces unsupported characters with `?`.
The Clipboard panel shows whether UTF-8 was negotiated through the documented
Extended Clipboard extension. Text is limited to 1 MiB; images, files, rich text,
embedded NULs, and unsupported formats are not transferred.

VNC clipboard and desktop traffic are not encrypted. Use a trusted network or VPN.
Use **Clipboard** for encrypted Unicode clipboard access when the Mac does not offer
the UTF-8 VNC extension.

## SSH setup and identity

On the Mac, enable **System Settings > General > Sharing > Remote Login** and
allow your account. Screen Sharing alone does not provide SSH/SFTP.
Keep both services behind a VPN rather than exposing ports 5900 or 22 publicly.

Open **Clipboard** or **Files** in an active screen-sharing session. The host is the connected
Mac; the default SSH port is 22. Enter the Mac account login or choose its saved
login. A separate classic VNC password is not an SSH account password.

Both panels share one SSH connection. Clipboard keeps **Copy Mac text** and
**Send Windows text**, login, and host-key approval in place; it never redirects
to Files. Files contains the browser and transfer controls. Closing a panel
keeps an established SSH session open, so switching panels does not reconnect.

Before any password is read from the vault or sent over SSH:

1. The SSH handshake presents the Mac's host key.
2. An existing pinned key must match. A changed key is blocked, without a bypass.
3. A first-use key requires explicit confirmation. Compare the displayed
   SHA-256 fingerprint on the Mac through a trusted channel.
4. A saved login requires a fresh Windows Hello approval.

For an Ed25519 host key, run this in the Mac's Terminal:

```sh
ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub -E sha256
```

For ECDSA, use `/etc/ssh/ssh_host_ecdsa_key.pub`. The confirmation panel displays
the appropriate command. IP addresses, machine names, and a successful VNC
connection do not replace fingerprint verification.

Pins are public-key fingerprints in `ssh_host_keys` in the profile settings,
not passwords. For a legitimate key rotation, close MacCanvas, independently
verify the replacement on the Mac, back up the settings, and remove only the
matching host/port entry. The next connection will ask you to verify it again.
There is deliberately no “accept changed keys” switch.

## Files

- Browse the Mac account's folders; enter a path directly or use **Up**.
- Choose **Send a file...** to upload to the displayed folder. The optional
  name field lets you choose a different destination name.
- Choose **Receive** beside a file to select a new Windows destination.
- Existing files are never replaced. Folders, symlinks, and special files are
  not transferred or followed as file entries.
- Files are streamed in bounded chunks, with a 1 TiB per-file limit. A folder
  listing is limited to 2,048 entries; enter a smaller subfolder directly.
- Uploads use an owner-only temporary file, optionally fsync it when OpenSSH
  supports that extension, read it back, compare SHA-256, then perform a standard
  non-overwriting SFTP rename.
- Downloads use a local temporary file, check reported size/modification time,
  flush it, and publish it without replacing an existing file.
- Cancel and failure paths attempt to remove their temporary files. A cleanup
  failure is reported with the exact remaining path. Abrupt termination can
  leave a `.maccanvas-*.part` or `.maccanvas-download-*.part` file.

The progress display shows the current phase and effective file-data rate.
The main network graph includes VNC, HEVC/SRTP, and SSH traffic. The VNC bandwidth
target does not throttle SSH transfers.

### Permission denied

SSH login permission and file access are separate. If the Mac allows your home
or Public folder but denies Desktop, Documents or Downloads, check
**System Settings > General > Sharing > Remote Login > Allow full disk access
for remote users** on the Mac. Reconnect SSH after changing that setting.
Also check the folder's **Get Info > Sharing & Permissions** for the SSH account.
MacCanvas cannot override macOS privacy rules or filesystem permissions.

A denied folder leaves SSH connected and keeps the last accessible folder
visible. Enter another path to continue. If the initial home folder is denied,
file sending stays disabled until an accessible folder has been opened.

Downloads produce a SHA-256 receipt of the received stream, not an independently
signed checksum or an APFS snapshot. Avoid transferring a file while another
application is modifying it. A trusted host key authenticates the server; it
does not prove that the server or its filesystem is uncompromised.

## Encrypted Unicode clipboard

After connecting SSH, use **Copy Mac text** or **Send Windows text**.
These are explicit operations, independent of automatic VNC clipboard sharing.
Use the same Mac account as the shared desktop. macOS may restrict pasteboard
access when that account has no usable logged-in desktop session.

MacCanvas invokes two fixed commands through the encrypted SSH session. They use
Apple's built-in JavaScript for Automation and AppKit `NSPasteboardTypeString`.
Text travels through standard input/output, never through an interpolated shell
command, argument, temporary helper file, or audit record. This preserves literal
RTF/PostScript-looking text instead of treating it as rich clipboard content.
No general-purpose remote-command interface is exposed.

Saved passwords remain protected by the same mandatory Hello gate used for VNC.
Clipboard audit events contain direction, encoding, byte counts, and outcomes.
Helper failures also report an allowlisted operation stage, error category and
numeric code, and whether SSH matches a logged-in console desktop account.
These distinguish missing plain text from helper errors without assuming a
macOS permission restriction. Raw helper output and clipboard text are never
displayed as diagnostics or written to the audit log.

## Protocol references

- [Apple: Share another Mac's screen, clipboard, and files](https://support.apple.com/guide/mac-help/share-the-screen-of-another-mac-mh14066/mac)
- [RFB protocol: CutText and Extended Clipboard](https://github.com/rfbproto/rfbproto/blob/master/rfbproto.rst#extended-clipboard-pseudo-encoding)
- [Apache Guacamole: VNC clipboard encoding](https://guacamole.apache.org/doc/gug/configuring-guacamole.html#clipboard-encoding)
- [SFTP version 3 specification](https://datatracker.ietf.org/doc/html/draft-ietf-secsh-filexfer-02)
- [Apple NSPasteboard](https://developer.apple.com/documentation/appkit/nspasteboard)
- [iShareScreen: Apple High Performance protocol](https://github.com/renegadelink/iShareScreen/blob/9ab40d3a3151524f954cff9d6239d891197c1705/docs/apple_vnc_rfc.md)

Apple documents richer Mac-to-Mac sharing, but those features are not a portable
standard-RFB file-transfer contract. Extended Clipboard's file-format bit is
reserved, not a defined file-transfer protocol. MacCanvas does not advertise
UltraVNC, TightVNC, or RealVNC vendor file-transfer formats as Apple-compatible.
