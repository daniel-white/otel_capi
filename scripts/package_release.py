#!/usr/bin/env python3
"""Package a Cargo release build into a distributable otel-capi archive."""

from __future__ import annotations

import argparse
import hashlib
import re
import shutil
import tarfile
import tempfile
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
STATIC_LIBRARIES = (
    "libotel_capi.a",
    "otel_capi.lib",
)
SHARED_LIBRARIES = (
    "libotel_capi.so",
    "libotel_capi.dylib",
    "libotel_capi_grpc_exporter.so",
    "libotel_capi_grpc_exporter.dylib",
    "libotel_capi_http_exporter.so",
    "libotel_capi_http_exporter.dylib",
    "libotel_capi_stdout_exporter.so",
    "libotel_capi_stdout_exporter.dylib",
    "otel_capi.dll.lib",
    "otel_capi.dll",
    "otel_capi_grpc_exporter.dll.lib",
    "otel_capi_grpc_exporter.dll",
    "otel_capi_http_exporter.dll.lib",
    "otel_capi_http_exporter.dll",
    "otel_capi_stdout_exporter.dll.lib",
    "otel_capi_stdout_exporter.dll",
)
LIBRARY_NAMES = STATIC_LIBRARIES + SHARED_LIBRARIES
DOC_FILES = ("LICENSE", "README.md", "CHANGELOG.md", "LINKING.md")


def cargo_version() -> str:
    text = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    match = re.search(r'(?m)^version\s*=\s*"([^"]+)"', text)
    if not match:
        raise SystemExit("could not read package version from Cargo.toml")
    return match.group(1)


def clean_version(value: str) -> str:
    return value[1:] if value.startswith("v") else value


def render_pc(template: Path, version: str) -> str:
    return (
        template.read_text(encoding="utf-8")
        .replace("@PREFIX@", "${pcfiledir}/../..")
        .replace("@VERSION@", version)
    )


def stage_tree(
    target_dir: Path, version: str, target: str, libraries_to_stage: tuple[str, ...]
) -> Path:
    stage = Path(tempfile.mkdtemp(prefix=f"otel-capi-{version}-{target}-"))
    package = stage / f"otel-capi-{version}-{target}"
    (package / "include").mkdir(parents=True)
    (package / "lib" / "pkgconfig").mkdir(parents=True)
    (package / "bin").mkdir()

    headers = sorted(ROOT.glob("include/otel_capi*.h"))
    if not headers:
        raise SystemExit("no generated headers found; run cargo build first")
    for header in headers:
        shutil.copy2(header, package / "include" / header.name)

    libraries = [
        target_dir / name for name in libraries_to_stage if (target_dir / name).is_file()
    ]
    if not libraries:
        raise SystemExit(
            f"no otel_capi libraries for this variant found in {target_dir}"
        )
    for library in libraries:
        destination = package / ("bin" if library.name.endswith(".dll") else "lib")
        shutil.copy2(library, destination / library.name)

    for name in DOC_FILES:
        source = ROOT / name
        if source.is_file():
            shutil.copy2(source, package / name)

    pc = package / "lib" / "pkgconfig" / "otel-capi.pc"
    pc.write_text(render_pc(ROOT / "otel-capi.pc.in", version), encoding="utf-8")
    return stage


def create_archive(
    stage: Path, out_dir: Path, version: str, target: str, suffix: str = ""
) -> Path:
    package_name = f"otel-capi-{version}-{target}"
    archive_name = f"{package_name}{suffix}"
    package = stage / package_name
    if "windows" in target:
        archive = out_dir / f"{archive_name}.zip"
        with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as bundle:
            for path in sorted(package.rglob("*")):
                if path.is_file():
                    bundle.write(path, path.relative_to(stage))
    else:
        archive = out_dir / f"{archive_name}.tar.gz"
        with tarfile.open(archive, "w:gz") as bundle:
            bundle.add(package, arcname=archive_name)
    return archive


def write_checksum(archive: Path) -> Path:
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    checksum = archive.with_suffix(archive.suffix + ".sha256")
    checksum.write_text(f"{digest}  {archive.name}\n", encoding="utf-8")
    return checksum


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True, help="Cargo target triple")
    parser.add_argument("--version", help="Release version or tag (defaults to Cargo.toml)")
    parser.add_argument("--target-dir", help="Cargo release output directory")
    parser.add_argument("--out-dir", default="dist", help="Directory for archives")
    args = parser.parse_args()

    version = clean_version(args.version or cargo_version())
    target_dir = Path(args.target_dir or f"target/{args.target}/release")
    if not target_dir.is_absolute():
        target_dir = ROOT / target_dir
    out_dir = Path(args.out_dir)
    if not out_dir.is_absolute():
        out_dir = ROOT / out_dir
    out_dir.mkdir(parents=True, exist_ok=True)

    variants = (
        (LIBRARY_NAMES, ""),
        (STATIC_LIBRARIES, "-static"),
        (SHARED_LIBRARIES, "-shared"),
    )
    for libraries, suffix in variants:
        stage = stage_tree(target_dir, version, args.target, libraries)
        try:
            archive = create_archive(stage, out_dir, version, args.target, suffix)
            checksum = write_checksum(archive)
        finally:
            shutil.rmtree(stage)

        print(archive)
        print(checksum)


if __name__ == "__main__":
    main()
