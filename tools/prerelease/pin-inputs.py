#!/usr/bin/env python3
"""Validate public guest assets and write the reviewed manifest pins."""

import hashlib
import json
import pathlib
import re
import sys

repo, tag, assets_dir, image_build = sys.argv[1:]
assets = pathlib.Path(assets_dir)
root = pathlib.Path(__file__).resolve().parents[2]
if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repo):
    raise SystemExit("invalid repository")
if not re.fullmatch(r"fixtures-[0-9a-f]{40}", tag):
    raise SystemExit("fixture tag must bind a full source commit")
if not re.fullmatch(r"[A-Za-z0-9.]+", image_build):
    raise SystemExit("invalid macOS build")


def sha256(path):
    h = hashlib.sha256()
    with path.open("rb") as f:
        while data := f.read(1024 * 1024):
            h.update(data)
    return h.hexdigest()


checks = {}
for line in (assets / "SHA256SUMS").read_text().splitlines():
    m = re.fullmatch(r"([0-9a-f]{64})  ([A-Za-z0-9.-]+)", line)
    if m is None or m[2] in checks:
        raise SystemExit("invalid fixture checksum list")
    checks[m[2]] = m[1]
names = {"fixture-oci.tar", "fixture-docker.tar", "provenance.json", "base-image.txt"}
if set(checks) != names:
    raise SystemExit("fixture checksum list is incomplete")
for name in names:
    if sha256(assets / name) != checks[name]:
        raise SystemExit(f"fixture checksum mismatch: {name}")
if (assets / "base-image.txt").read_text() != (root / "ci/fixture-base-image.txt").read_text():
    raise SystemExit("published base image differs from the reviewed pin")
provenance = json.loads((assets / "provenance.json").read_text())
if provenance.get("schema") != 1 or provenance.get("platform") != {
    "os": "linux", "architecture": "arm64"
}:
    raise SystemExit("fixture provenance has the wrong platform or schema")
if provenance.get("recipe") != "guest-contract-v1" or provenance.get("recipe_sha256") != sha256(
    root / "xtask/src/fixtures/recipes/guest_image.rs"
) or provenance.get("image_contract_sha256") != sha256(
    root / "tests/guest/support/image-contract.sh"
):
    raise SystemExit("fixture provenance differs from the reviewed recipe or contract")
if provenance.get("archives") != {
    "fixture-oci.tar": checks["fixture-oci.tar"],
    "fixture-docker.tar": checks["fixture-docker.tar"],
}:
    raise SystemExit("fixture provenance differs from archive checksums")

manifest = root / "ci/gates.toml"
text = manifest.read_text()
for fixture, asset in (("guest-oci-base", "fixture-oci.tar"), ("guest-docker-base", "fixture-docker.tar")):
    pattern = rf"(?ms)^(\[fixtures\.{fixture}\]\n)(.*?)(?=^\[|\Z)"
    m = re.search(pattern, text)
    if m is None:
        raise SystemExit(f"missing {fixture} table")
    lines = [line for line in m[2].splitlines() if not re.match(r"^(pending|sha256|source)\s*=", line)]
    while lines and not lines[-1]:
        lines.pop()
    body = "\n".join(lines) + "\n"
    body += f'sha256 = "{checks[asset]}"\n'
    body += f'source = "https://github.com/{repo}/releases/download/{tag}/{asset}"\n\n'
    text = text[:m.start()] + m[1] + body + text[m.end():]
for platform in ("macos-arm64-ci-guest", "macos-arm64-release"):
    pattern = rf"(?ms)^(\[platforms\.{platform}\]\n)(.*?)(?=^\[|\Z)"
    m = re.search(pattern, text)
    if m is None:
        raise SystemExit(f"missing {platform} table")
    old = re.search(r'(?m)^image = "([^"]+)"$', m[2])
    if old is None or not (old[1].startswith("pending") or old[1].startswith("macos-")):
        raise SystemExit(f"refusing to replace existing {platform} image pin")
    body = m[2].replace(old[0], f'image = "macos-{image_build}"', 1)
    text = text[:m.start()] + m[1] + body + text[m.end():]
manifest.write_text(text)
print("Guest fixtures and both Mac runner images pinned in ci/gates.toml")
