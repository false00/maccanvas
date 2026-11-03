# Native Apple audio fixture

`apple-aac-eld-stereo.aaceld` contains 100 original synthetic stereo packets,
encoded with Apple's native AudioToolbox AAC-ELD-SBR encoder: 48 kHz, 480 samples
per packet, left 440 Hz, right 880 Hz, amplitude 0.1, target 80 kbps.
Each raw access unit has a two-byte big-endian size prefix.
AudioSpecificConfig: `f8e65132e000`.

SHA-256:
`99954e53e6a6646b982f223ac248a0af059230eef468934c03c2ffa44d03dd04`

Generated on macOS 15 by
[the native fixture job](https://github.com/false00/maccanvas/actions/runs/41540897246).
The [generator](../../scripts/generate_macos_audio_fixture.c) creates its input
mathematically. It does not record user audio, screen content, or a microphone.
The fixture and generator are AGPL-3.0-or-later, like the viewer.

On macOS, compile the generator with:

```sh
clang -std=c11 -O2 scripts/generate_macos_audio_fixture.c -framework AudioToolbox -framework AudioUnit -framework CoreAudio -o native-audio-fixture
./native-audio-fixture stereo.aaceld
```

The Windows tests require the real six-byte Apple configuration and measure
decoded tone amplitudes, channel separation, rate, and complete sample count.
The encrypted mock-Mac test also authenticates these packets through SRTP.
