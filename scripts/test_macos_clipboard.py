"""Verify the shipped clipboard commands against real AppKit on an isolated CI Mac."""

import os
from pathlib import Path
import re
import subprocess
import sys


def command(name):
    source = (Path(__file__).resolve().parent.parent / "src" / "files.rs").read_text(
        encoding="utf-8"
    )
    match = re.search(
        rf"const\s+{name}:\s*&str\s*=\s*r#\"(.*?)\"#;", source, re.DOTALL
    )
    if match is None:
        raise RuntimeError(f"Cannot locate the shipped {name} command")
    return match.group(1)


def invoke(script, data=b""):
    result = subprocess.run(
        ["/bin/sh", "-c", script],
        input=data,
        capture_output=True,
        timeout=20,
        check=False,
    )
    if result.returncode:
        raise RuntimeError(
            f"Native Mac clipboard helper exited {result.returncode}: "
            f"{result.stderr.decode('utf-8', errors='replace')}"
        )
    return result.stdout


def main():
    if sys.platform != "darwin" or os.environ.get("GITHUB_ACTIONS") != "true":
        raise RuntimeError(
            "Run this test only on an isolated GitHub Actions Mac. "
            "It deliberately replaces that test machine's clipboard."
        )
    read = command("READ_CLIPBOARD")
    write = command("WRITE_CLIPBOARD")
    samples = [
        "",
        "MacCanvas Unicode: caf\u00e9 \u03bb \U0001f34e\nsecond line",
        "{\\rtf1 This must remain literal plain text.}",
        "%!PS-Adobe-3.0 EPSF-3.0\nThis must remain literal plain text.",
        "MacCanvas clipboard test\n" * 10_000,
    ]
    for sample in samples:
        encoded = sample.encode("utf-8")
        assert invoke(write, encoded) == b"", "A write must not echo clipboard contents"
        assert invoke(read) == encoded, "Native Mac clipboard content changed"
    invoke(write, b"")
    print(
        "PASS: real macOS AppKit clipboard; Unicode, empty text, literal RTF/EPS, "
        "large text, and no content echoed by writes."
    )


if __name__ == "__main__":
    main()
