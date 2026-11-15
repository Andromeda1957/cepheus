#!/usr/bin/env python3
"""Export committed public Cepheus source, refusing any private payload."""

import argparse
import shutil
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path
from pathlib import PurePosixPath

ROOT = Path(__file__).resolve().parents[1]


def export_source(output, ref="HEAD"):
    # Archive a subtree, rather than its Git history: private files never enter
    # the new repository. The subtree's .gitattributes supplies export-ignore.
    if not ref or ref.startswith("-") or ":" in ref:
        raise ValueError("Use a commit or branch name for --ref")
    with tempfile.TemporaryFile() as stream:
        subprocess.run(
            [
                "git",
                "-C",
                str(ROOT),
                "archive",
                "--format=tar",
                "--prefix=cepheus/",
                ref,
            ],
            stdout=stream,
            check=True,
        )
        stream.seek(0)
        with tarfile.open(fileobj=stream, mode="r:") as archive:
            for member in archive:
                path = PurePosixPath(member.name)
                if (
                    path.is_absolute()
                    or ".." in path.parts
                    or not path.parts
                    or path.parts[0] != "cepheus"
                ):
                    raise ValueError("Invalid public archive member")
                if len(path.parts) > 1 and path.parts[1] == "private":
                    raise ValueError("Refusing public export: private files are present")
        stream.seek(0)
        # Never overwrite an existing archive or follow an output symlink.
        with output.open("xb") as destination:
            shutil.copyfileobj(stream, destination)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True, help="New .tar archive")
    parser.add_argument("--ref", default="HEAD", help="Committed source revision")
    args = parser.parse_args()
    try:
        export_source(args.output, args.ref)
    except (OSError, ValueError, subprocess.CalledProcessError, tarfile.TarError) as error:
        print(f"Public source export: {error}", file=sys.stderr)
        return 1
    print(f"Public Cepheus source: {args.output}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
