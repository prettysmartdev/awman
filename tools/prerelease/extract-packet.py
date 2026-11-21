#!/usr/bin/env python3
"""Extract an untrusted native packet without links or path traversal."""

import pathlib
import sys
import tarfile

archive = pathlib.Path(sys.argv[1])
destination = pathlib.Path(sys.argv[2])
total = 0
seen = set()
with tarfile.open(archive, "r:gz") as source:
    members = source.getmembers()
    for member in members:
        parts = pathlib.PurePosixPath(member.name).parts
        if not parts or parts[0] != "evidence" or any(p in ("", ".", "..") for p in parts):
            raise SystemExit(f"invalid packet member: {member.name}")
        if member.name in seen or not (member.isdir() or member.isfile()):
            raise SystemExit(f"duplicate or special packet member: {member.name}")
        seen.add(member.name)
        total += member.size
        if total > 4 * 1024**3:
            raise SystemExit("packet exceeds the 4 GiB extraction limit")
    for member in members:
        path = destination.joinpath(*pathlib.PurePosixPath(member.name).parts)
        if member.isdir():
            path.mkdir(parents=True, exist_ok=True)
        else:
            path.parent.mkdir(parents=True, exist_ok=True)
            input_file = source.extractfile(member)
            if input_file is None:
                raise SystemExit(f"missing packet file: {member.name}")
            with input_file, path.open("xb") as output_file:
                while chunk := input_file.read(1024 * 1024):
                    output_file.write(chunk)
