#!/usr/bin/env python3
"""Upload packaged release assets with the GitHub CLI."""

from __future__ import annotations

import argparse
import subprocess
from pathlib import Path

PATTERNS = ("*.tar.gz", "*.tar.gz.sha256", "*.zip", "*.zip.sha256")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag", required=True, help="Release tag to upload to")
    parser.add_argument("--dir", default="dist", help="Directory containing release assets")
    args = parser.parse_args()

    asset_dir = Path(args.dir)
    assets = sorted({path for pattern in PATTERNS for path in asset_dir.glob(pattern)})
    if not assets:
        raise SystemExit(f"no release assets found in {asset_dir}")

    command = ["gh", "release", "upload", args.tag, *(str(asset) for asset in assets), "--clobber"]
    subprocess.run(command, check=True)


if __name__ == "__main__":
    main()
