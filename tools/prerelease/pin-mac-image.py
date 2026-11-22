#!/usr/bin/env python3
"""Update the two self hosted Mac image records for a reviewed OS build."""

import pathlib
import re
import sys

build = sys.argv[1]
if not re.fullmatch(r"[A-Za-z0-9.]+", build):
    raise SystemExit("invalid macOS build")
manifest = pathlib.Path(__file__).resolve().parents[2] / "ci/gates.toml"
text = manifest.read_text()
for platform in ("macos-arm64-ci-guest", "macos-arm64-release"):
    pattern = rf"(?ms)^(\[platforms\.{platform}\]\n)(.*?)(?=^\[|\Z)"
    table = re.search(pattern, text)
    if table is None:
        raise SystemExit(f"missing {platform} table")
    image = re.search(r'(?m)^image = "([^"]+)"$', table[2])
    if image is None or not (image[1].startswith("pending") or image[1].startswith("macos-")):
        raise SystemExit(f"unrecognized {platform} image pin")
    body = table[2].replace(image[0], f'image = "macos-{build}"', 1)
    text = text[:table.start()] + table[1] + body + text[table.end():]
manifest.write_text(text)
