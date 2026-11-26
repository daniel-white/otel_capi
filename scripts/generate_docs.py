#!/usr/bin/env python3
"""Generate Doxygen HTML documentation for the generated C headers."""

from __future__ import annotations

import argparse
import re
import shutil
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def cargo_version() -> str:
    text = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    match = re.search(r'(?m)^version\s*=\s*"([^"]+)"', text)
    if not match:
        raise SystemExit("could not read package version from Cargo.toml")
    return match.group(1)


def render_doxyfile(version: str, out_dir: Path, headers: list[Path]) -> str:
    inputs = " ".join(f'"{path}"' for path in [ROOT / "README.md", *headers])
    return f"""PROJECT_NAME           = "otel-capi"
PROJECT_NUMBER         = "{version}"
PROJECT_BRIEF          = "C API for OpenTelemetry"
OUTPUT_DIRECTORY       = "{out_dir}"
INPUT                  = {inputs}
USE_MDFILE_AS_MAINPAGE = "{ROOT / 'README.md'}"
FILE_PATTERNS          = *.h *.md
RECURSIVE              = NO
EXTRACT_ALL            = YES
EXTRACT_STATIC         = YES
JAVADOC_AUTOBRIEF      = YES
ENABLE_PREPROCESSING   = YES
MACRO_EXPANSION        = YES
EXPAND_ONLY_PREDEF     = YES
PREDEFINED             = OTEL_API=
INCLUDE_PATH           = "{ROOT / 'include'}"
GENERATE_HTML          = YES
HTML_OUTPUT            = html
FULL_SIDEBAR           = YES
GENERATE_TREEVIEW      = YES
GENERATE_LATEX         = NO
GENERATE_MAN           = NO
GENERATE_RTF           = NO
GENERATE_XML           = NO
QUIET                  = YES
WARNINGS               = YES
WARN_IF_UNDOCUMENTED   = NO
WARN_IF_DOC_ERROR      = YES
WARN_NO_PARAMDOC       = NO
"""


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out-dir", default="build/docs", help="Documentation output directory")
    parser.add_argument("--version", help="Version to display (defaults to Cargo.toml)")
    parser.add_argument("--doxygen", default="doxygen", help="Doxygen executable")
    args = parser.parse_args()

    doxygen = shutil.which(args.doxygen)
    if not doxygen:
        raise SystemExit(f"doxygen not found: {args.doxygen}")

    headers = sorted(ROOT.glob("include/otel_capi*.h"))
    if not headers:
        raise SystemExit("no generated headers found; run cargo build first")

    out_dir = Path(args.out_dir)
    if not out_dir.is_absolute():
        out_dir = ROOT / out_dir
    out_dir.mkdir(parents=True, exist_ok=True)

    config = out_dir / "Doxyfile"
    config.write_text(
        render_doxyfile(args.version or cargo_version(), out_dir, headers),
        encoding="utf-8",
    )
    subprocess.run([doxygen, str(config)], cwd=ROOT, check=True)

    print(out_dir / "html")


if __name__ == "__main__":
    main()
