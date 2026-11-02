# Local security audit

MacCanvas records structured local evidence for troubleshooting and incident
response. It is not an endpoint-monitoring agent and does not upload logs.

## Location and access

The default file is **`~\.maccanvas\log.json`**. Despite its `.json` extension,
it is **JSONL**: compact UTF-8 JSON, one record per line, with no outer array.
**About > Open log folder** opens its location.

The directory, lock file, primary log, and recognized archives have a protected
Windows DACL allowing the current user SID and LocalSystem. Logs are not
encrypted. Administrators can take ownership; software running as the same user
can access or alter them. Treat logs as sensitive evidence.

`--data-dir DIRECTORY` isolates profiles and logs for portable/testing use:
the log is `DIRECTORY\audit\log.json`. Passing the normal default profile folder
is treated as a normal launch, including when upgrading from older versions.
Profiles normally remain in `%LOCALAPPDATA%\MacCanvas\settings.json`;
passwords remain in Windows Credential Manager.

Uninstalling the viewer does not erase audit evidence.

## Evidence collected

| Area | Evidence |
|---|---|
| Application | Version, UTC build timestamp and timestamp source, complete source revision, dirty/archive state, target, Rust compiler, executable path and on-disk SHA-256 |
| Windows context | Hostname, OS version, process ID/creation time, user SID, logon LUID, Windows session ID, elevation, integrity RID/level |
| Launch lineage | Windows-reported parent PID, up to eight ancestors, observed image paths and creation times, direct parent's on-disk SHA-256 |
| Connections | Profile UUID, configured host/port, actual local/remote socket endpoints, protocol, authentication source/type and outcomes, duration, byte totals |
| Consent and vault | Hello requested/completed/denied, safe local denial reasons, credential read/save/delete requests and outcomes; no secret values |
| SSH trust | Host-key algorithm, SHA-256 fingerprint, first-use approval/pinning, changed-key blocks before credential access |
| HEVC | Authentication/control/media negotiation stages, codec and geometry, actual first decoded frame, safe RTP/reassembly counters, keyframe recovery and decode failures; no keys, encoded payloads, or images |
| Operations | Settings persistence, directory-browse path/count/failures, numeric SFTP status (`error.code`, `sftp.status_code`), file-transfer requests/commit/completion/cancellation, selected paths, sizes, source modification time when available, SHA-256 receipts, temporary-file paths and cleanup failures |
| Updates | Official check outcomes, expected artifact version/size/hash, verification/preflight, replacement and recovery outcomes |
| Clipboard | Direction, encoding, byte counts, accepted/ignored/applied outcomes; never clipboard data or hashes |
| Failures | Error category and native I/O code when available, audit-health failures, panic source location/thread without the panic payload |

Process ancestry is collected through Windows process/token APIs, not through
PowerShell, WMI scripts, or broad process-content surveillance. Other processes
are enumerated only to resolve this application's ancestry. Unavailable,
access-denied, hash-limit, and PID-reuse observations are explicit. Creation
order checks are not proof of causal parentage: Windows parent PIDs can be
spoofed. Image hashes describe files on disk, not verified process memory.

Parent command lines, raw application arguments, environment variables,
passwords, Windows Hello PINs/biometrics, vault blobs, keystrokes, framebuffer
pixels, clipboard contents/hashes, and file contents are not recorded.
File names, endpoints, SIDs, ancestry, sizes, and **transferred-file** hashes
are sensitive metadata and can still identify people or documents.

## Schema

The schema is OpenTelemetry-oriented, **not an OTLP export format**.
Its dotted resource/attribute names and trace/span IDs make mapping to a
collector or SIEM straightforward. No telemetry SDK, exporter, or external
collector is required.

| Field | Meaning |
|---|---|
| `schema_version` | MacCanvas log schema, currently `1` |
| `timestamp` | UTC RFC 3339 event time from the Windows clock |
| `observed_timestamp` | UTC time when the writer handled the event |
| `severity_number`, `severity_text` | OpenTelemetry-style INFO `9`, WARN `13`, ERROR `17`, FATAL `21` |
| `event_name`, `body` | Stable event identifier |
| `resource` | Service/build/process/host/identity evidence; `service.instance.id` identifies this logger run |
| `attributes` | Operation-specific evidence and caller Windows thread ID/name |
| `trace_id`, `span_id`, `trace_flags` | Correlation IDs; flags are `0`, with no claim that an external trace was sampled/exported |
| `log.record.uid` | Unique record ID inside the `log` object |
| `log.record.sequence` | Per-run sequence, including across file rotation and concurrent instances |
| `log.audit.elapsed_ns` | Monotonic nanoseconds since the logger became ready, represented as a decimal string |
| `log.file.offset` | Byte offset of this record in the current primary file |
| `log.records_lost_since_last_write` | Queue/write loss observed before the next successful record |
| `integrity` | Algorithm, previous per-run record hash, and current record hash |

Wall-clock timestamps are not independently trusted time. Compare them with
monotonic elapsed time and other evidence if clock changes are suspected.
Network summaries are low-volume aggregates, not packet or input recordings.
Clipboard and file payloads never enter the logger.

## Persistence and retention

- One bounded background writer per instance, with a 1,024-event queue.
- Inter-process file-range locking serializes append, recovery, and rotation.
- Each written record is flushed to disk. High-value lifecycle, consent, vault,
  authentication, update, and transfer-commit events wait for persistence.
- New network/credential operations fail closed when logging cannot be
  initialized or its health is lost; active connections are stopped and the UI
  reports a persistent warning. Read-only log-folder access remains available.
- Records are limited to 256 KiB. The primary file rotates before exceeding
  16 MiB, retaining seven recognized archives: approximately 128 MiB total.
- Rotation and removed archive names are recorded. Only the application's
  timestamp/UUID archive naming pattern is eligible for retention deletion.
- An incomplete crash tail is preserved by renaming the whole affected file to
  a `.partial` archive before creating a new valid primary stream. It remains
  subject to the same archive-count limit.

Normal shutdown drains queued records. Abrupt process termination, a full disk,
hardware failure, or saturation can lose queued/in-flight evidence. This is not
a guarantee of lossless logging, an immutable journal, or a crash-dump system.
Logging failures never quietly turn auditing off and allow saved-password use.

## Integrity verification

Close MacCanvas and collect a consistent copy before analysis. Preserve the
primary log and archives together; record collection hashes and custody outside
the application.

To check the current JSONL file:

```powershell
.\MacCanvas.exe --verify-log "$HOME\.maccanvas\log.json"
```

For a retained cross-rotation chain, pass archived `.jsonl` files in chronological
order followed by `log.json`. `.partial` files intentionally preserve incomplete
raw evidence and are not ordinary valid JSONL inputs.

The verifier checks record shape, correlation IDs, timestamps, SHA-256 hashes,
and per-run sequence/previous-hash links. A file can legitimately begin mid-run
after rotation. The hash input is compact UTF-8 JSON with recursively sorted
object keys and `integrity.hash` omitted. `integrity.previous_hash` is included.
Use the application's verifier rather than assuming another JSON serializer
will produce identical floating-point or escaping representations.

**Hashes are not signatures.** An attacker who can rewrite a log can recompute
its hashes. Without an independently preserved anchor, removing an entire
prefix, suffix, or run may not be detectable. Integrity checks help identify
accidental edits and broken retained chains; they do not establish authorship,
nonrepudiation, trusted timestamps, or protection against the user/administrator.

PowerShell 5.1+ can read JSONL one line at a time:

```powershell
Get-Content "$HOME\.maccanvas\log.json" |
    ForEach-Object { $_ | ConvertFrom-Json } |
    Select-Object timestamp, severity_text, event_name, trace_id, span_id, attributes
```

Sanitize metadata before sharing excerpts. Never publish an entire real log as
an issue attachment by default.
