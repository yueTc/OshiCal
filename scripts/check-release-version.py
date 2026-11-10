#!/usr/bin/env python3
import json
import re
import sys
from pathlib import Path


def semver(value):
    match = re.fullmatch(
        r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
        r"(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?"
        r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?",
        value,
    )
    if not match:
        raise SystemExit(f"invalid SemVer: {value!r}")
    core = tuple(int(match.group(index)) for index in range(1, 4))
    prerelease = match.group(4)
    if prerelease is None:
        return core + ((1,),)
    parts = []
    for part in prerelease.split("."):
        parts.append((0, int(part)) if part.isdigit() else (1, part))
    return core + ((0, tuple(parts)),)


candidate_text = sys.argv[1]
candidate = semver(candidate_text)
for manifest_path in sys.argv[2:]:
    manifest = json.loads(Path(manifest_path).read_text(encoding="utf-8"))
    existing_text = manifest.get("version")
    if not isinstance(existing_text, str):
        raise SystemExit(f"manifest {manifest_path} is missing a string version")
    existing = semver(existing_text)
    if candidate <= existing:
        raise SystemExit(
            f"release version {candidate_text} is not newer than {existing_text} in {manifest_path}"
        )
print(f"release version {candidate_text} is newer than {len(sys.argv) - 2} published manifest(s)")
