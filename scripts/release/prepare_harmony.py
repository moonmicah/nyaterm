#!/usr/bin/env python3
"""Stage the pinned HarmonyOS hdc toolchain from the official Command Line Tools archive.

The archive is distributed by Huawei's download centre, which requires a signed-in
account, so unlike Android's platform-tools it cannot be fetched by an anonymous
build. Download it once, point ``NYATERM_HARMONY_CLI_ZIP`` at it (or drop it in
``~/Downloads`` / ``<staging>/downloads``), and this module extracts just the
toolchain files we ship. The extracted ~7 MB staging tree is the only input the
packager needs afterwards, so the multi-gigabyte archive can be removed.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import zipfile

ROOT = Path(__file__).resolve().parents[2]
METADATA_PATH = Path(__file__).with_name("harmony-package.json")
METADATA = json.loads(METADATA_PATH.read_text(encoding="utf-8"))
STAGING = Path(
    os.environ.get("NYATERM_HARMONY_DEPS_DIR", ROOT / "target" / "harmony-deps")
).resolve()
WINDOWS_TARGET = "x86_64-pc-windows-msvc"
LICENSE_NAME = "LICENSE.txt"


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(8 * 1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def archive_candidates(explicit: str | None) -> list[Path]:
    candidates: list[Path] = []
    if explicit:
        candidates.append(Path(explicit))
    if os.environ.get("NYATERM_HARMONY_CLI_ZIP"):
        candidates.append(Path(os.environ["NYATERM_HARMONY_CLI_ZIP"]))
    candidates.append(STAGING / "downloads" / METADATA["archive"])
    downloads = Path.home() / "Downloads"
    if downloads.is_dir():
        candidates.extend(sorted(downloads.glob("commandline-tools-windows-x64-*.zip")))
    return [path for path in candidates if path.is_file()]


def find_archive(explicit: str | None = None) -> Path:
    for path in archive_candidates(explicit):
        if path.stat().st_size == METADATA["size"]:
            return path.resolve()
    found = ", ".join(str(path) for path in archive_candidates(explicit)) or "none"
    raise FileNotFoundError(
        f"Command Line Tools archive {METADATA['archive']} "
        f"({METADATA['size']} bytes) not found; candidates checked: {found}. "
        "Download it from Huawei's download centre and set NYATERM_HARMONY_CLI_ZIP."
    )


def verify_archive(path: Path) -> None:
    if sha256(path) != METADATA["sha256"]:
        raise ValueError(f"Command Line Tools checksum mismatch: {path}")


def staged_files(output: Path) -> dict[str, str]:
    names = [*METADATA["files"], LICENSE_NAME]
    return {name: hashlib.sha256((output / name).read_bytes()).hexdigest() for name in names}


def prepare(target: str = WINDOWS_TARGET, *, archive: str | None = None) -> Path | None:
    """Return the staged ``hdc/`` directory, or None for non-Windows targets.

    Huawei publishes one x64 toolchain; Windows ARM64 runs it through the
    system x64 emulator, the same way the x86 adb.exe ships on every Windows
    architecture.
    """
    if "windows" not in target:
        return None
    output = STAGING / "hdc" / target
    stamp = output / "SOURCE.json"
    if stamp.is_file():
        try:
            document = json.loads(stamp.read_text(encoding="utf-8"))
            if document.get("dependencies") == METADATA and document.get("files") == staged_files(output):
                return output
        except (OSError, ValueError):
            pass

    source = find_archive(archive)
    verify_archive(source)
    output.mkdir(parents=True, exist_ok=True)
    prefix = METADATA["toolchains"]["prefix"]
    with zipfile.ZipFile(source) as bundle:
        for name in METADATA["files"]:
            (output / name).write_bytes(bundle.read(prefix + name))
        (output / LICENSE_NAME).write_bytes(bundle.read(METADATA["license"]))
    stamp.write_text(
        json.dumps(
            {
                "target": target,
                "tool": METADATA["tool"],
                "source": METADATA["source"],
                "archive": METADATA["archive"],
                "archive_sha256": METADATA["sha256"],
                "toolchains_version": METADATA["toolchains"]["version"],
                "api_version": METADATA["toolchains"]["api_version"],
                "dependencies": METADATA,
                "files": staged_files(output),
            },
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )
    return output


def ensure_staged(target: str) -> Path | None:
    """Entry point used by package_native.py; never contacts the network."""
    return prepare(target)


def stage_runtime(source: Path, destination: Path) -> None:
    shutil.copytree(source, destination / "hdc", dirs_exist_ok=True)


def write_resource_manifest(destination: Path, target: str) -> list[Path]:
    files = sorted(path for path in (destination / "hdc").rglob("*") if path.is_file())
    manifest = {
        "schema": 1,
        "target": target,
        "dependencies": METADATA,
        "files": {
            path.relative_to(destination).as_posix(): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in files
        },
    }
    output = destination / "harmony-resources.json"
    output.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    return [*files, output]


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", default=WINDOWS_TARGET)
    parser.add_argument("--zip", help="Path to the official Command Line Tools archive")
    args = parser.parse_args()
    output = prepare(args.target, archive=args.zip)
    if output is None:
        print(f"hdc is bundled for Windows targets only; nothing to stage for {args.target}")
        return
    print(f"Staged {len(list(output.iterdir()))} hdc files in {output}")
    for name, digest in staged_files(output).items():
        print(f"  {name}: {digest}")


if __name__ == "__main__":
    main()
