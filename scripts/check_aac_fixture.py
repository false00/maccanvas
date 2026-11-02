"""Verify original synthetic Apple AAC packets against a native decoder."""

import array
import math
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import wave


def main(decoder, fixture, config):
    remaining = Path(fixture).read_bytes()
    packets = []
    while remaining:
        if len(remaining) < 2:
            raise ValueError("Truncated fixture length")
        length = struct.unpack(">H", remaining[:2])[0]
        if not 0 < length <= 4096 or length + 2 > len(remaining):
            raise ValueError("Invalid fixture packet")
        packets.append(remaining[2:length + 2])
        remaining = remaining[length + 2:]
    asc = Path(config).read_bytes()
    if asc != bytes.fromhex("f8e65132e000"):
        raise ValueError("Unexpected Apple AAC configuration")
    with tempfile.TemporaryDirectory(prefix="aac-conformance-") as directory:
        directory = Path(directory)
        raw = directory / "audio.raw"
        metadata = directory / "audio.txt"
        decoded = directory / "audio.wav"
        raw.write_bytes(asc + b"".join(packets))
        lines = [
            f"-dec_info_init:{len(asc)}", "-g_track_count:1",
            f"-ia_mp4_stsz_entries:{len(packets)}",
            "-movie_time_scale:48000", "-media_time_scale:48000",
            f"-playTimeInSamples:{len(packets) * 480}",
            "-startOffsetInSamples:0", "-useEditlist:1",
        ] + [f"-ia_mp4_stsz_size:{len(packet)}" for packet in packets]
        metadata.write_text("\n".join(lines) + "\n")
        result = subprocess.run(
            [str(Path(decoder).resolve()), f"-ifile:{raw}", f"-imeta:{metadata}",
             f"-ofile:{decoded}", "-mp4:1", "-fs480:1", "-eld_testing:1"],
            capture_output=True, text=True, timeout=30,
        )
        if result.returncode:
            raise RuntimeError(result.stdout + result.stderr)
        with wave.open(str(decoded), "rb") as output:
            if (output.getnchannels(), output.getsampwidth(), output.getframerate()) != (2, 2, 48000):
                raise ValueError("Unexpected PCM output format")
            pcm = array.array("h", output.readframes(output.getnframes()))
        for channel, frequency, other in [(0, 440, 880), (1, 880, 440)]:
            signal = pcm[channel::2][1920:-960]
            def amplitude(hz):
                real = sum(value / 32768 * math.cos(2 * math.pi * hz * index / 48000)
                           for index, value in enumerate(signal))
                imaginary = sum(value / 32768 * math.sin(2 * math.pi * hz * index / 48000)
                                for index, value in enumerate(signal))
                return 2 * math.hypot(real, imaginary) / len(signal)
            level = amplitude(frequency)
            cross = amplitude(other)
            print(f"channel={channel}, expected={frequency}Hz, amplitude={level:.6f}, cross={cross:.6f}")
            if not 0.06 < level < 0.14 or cross >= 0.015:
                raise ValueError("AAC decoding did not preserve the synthetic tone or stereo separation")


if __name__ == "__main__":
    main(*sys.argv[1:])
