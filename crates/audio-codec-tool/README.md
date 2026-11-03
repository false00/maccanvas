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

## Licenses

Glue and build metadata: [0BSD](LICENSE).
The unchanged [FDK AAC source](vendor/fdk-aac) retains its
[Fraunhofer license](vendor/fdk-aac/NOTICE), including its patent-license terms.
See [distribution notices](DISTRIBUTION-NOTICES.txt).
