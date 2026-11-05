# AAC-to-WAVE converter

An independent Windows x64 command-line converter. Reads standard LOAS/LATM
AAC on stdin and writes 16-bit streaming PCM WAVE on stdout. Supports the
48 kHz stereo AAC-ELD-SBR used by Apple High Performance Screen Sharing.
It does not access the network, microphone, or sound output device.

```powershell
cmd /c "aac-to-wav.exe < input.loas > output.wav"
aac-to-wav.exe --version
aac-to-wav.exe --licenses
```

WAVE length fields are `0xffffffff` for streaming. A change of sample rate or
channel count, malformed input, or an I/O failure exits nonzero.

## Build

Install Visual Studio C++ Build Tools and CMake 3.15+. From this folder:

```powershell
cmake -S native -B build -A x64 -DFDK_ROOT="$pwd\vendor\fdk-aac" -DCMAKE_POLICY_VERSION_MINIMUM=3.5
cmake --build build --config Release --target aac-to-wav --parallel 4
```

The executable is `build\Release\aac-to-wav.exe`. Rename it to
`MacCanvas-audio-windows-x64.exe` for the optional MacCanvas release asset.
The viewer's Cargo build also builds this program and exports only
size/hash/location metadata, never codec symbols or embedded executable bytes.

## Release reuse

Viewer-only releases reuse the exact verified converter from the previous stable
release instead of rebuilding identical native sources with new timestamps.
[The preparation script](../../scripts/Prepare-AudioComponent.ps1) compares the component package version,
native sources, vendored sources and license inputs against that release tag.
Changed native inputs require an `audio-codec-tool` package-version bump and a
fresh source build. Bump that version (and the converter's `--version`) for
native build-policy changes too; Rust-only metadata plumbing is not converter code.

CI stages the reused asset through `MACCANVAS_AUDIO_COMPONENT` and
`MACCANVAS_AUDIO_COMPONENT_SHA256`. Cargo independently verifies its checksum and
x64 executable format before exporting the normal metadata. Without these
paired variables, development builds still compile the vendored source.
Runtime SHA-256 verification and the separately licensed distribution are unchanged.

After publishing a viewer-only update, the
[live update fixture](../../scripts/Test-LiveUpdate.ps1) accepts `-AudioComponent`
to verify that an installed converter's bytes, size and trusted pin survive the
real executable replacement and restart.

## Licenses

Glue and build metadata: [0BSD](LICENSE).
The unchanged [FDK AAC source](vendor/fdk-aac) retains its
[Fraunhofer license](vendor/fdk-aac/NOTICE), including its patent-license terms.
See [distribution notices](DISTRIBUTION-NOTICES.txt).
