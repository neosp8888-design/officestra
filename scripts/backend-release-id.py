#!/usr/bin/env python3
"""Calculate and verify the backend identity embedded in an app bundle."""

from __future__ import annotations

import argparse
import hashlib
import plistlib
import re
import subprocess
import sys
from pathlib import Path


IDENTITY_FORMAT = b"OFFICESTRA_BACKEND_RELEASE_ID_V4\0"
# Rust backend executables and the ONNX Runtime library they load. Like Node,
# the identity uses the pre-signing SHA256 and post-signing CDHash records so
# timestamped signature containers do not change it.
RUST_RUNTIME_FILES = (
    "officestra-backend",
    "officestra-result",
    "officestra-telegram",
    "officestra-terminal-hook",
    "officestra-voice-follow",
    "libonnxruntime.dylib",
)
REQUIRED_FILES = (
    "OFFICESTRARuntime/infra/compose.yaml",
    "OFFICESTRARuntime/rust/SHA256",
    "OFFICESTRARuntime/rust/CDHASH",
    "OfficeLLM_OfficeCore.bundle/characters.json",
)
REQUIRED_PRESENT_FILES = (
    *(f"OFFICESTRARuntime/rust/bin/{name}" for name in RUST_RUNTIME_FILES),
)
REQUIRED_TREES = (
    "OFFICESTRARuntime/database/migrations",
    "OFFICESTRARuntime/tools/antigravity-playwright",
    "OFFICESTRARuntime/licenses",
)


class ReleaseIdentityError(RuntimeError):
    pass


def _regular_files(root: Path) -> list[Path]:
    if not root.is_dir():
        raise ReleaseIdentityError(f"Required release directory is missing: {root}")
    files = sorted(
        (path for path in root.rglob("*") if path.is_file()),
        key=lambda path: path.relative_to(root).as_posix(),
    )
    if not files:
        raise ReleaseIdentityError(f"Required release directory is empty: {root}")
    for path in files:
        if path.is_symlink():
            raise ReleaseIdentityError(
                f"Release identity input must not be a symlink: {path}"
            )
    return files


def release_input_files(app_bundle: Path) -> tuple[Path, list[Path]]:
    resources = app_bundle / "Contents" / "Resources"
    if not resources.is_dir():
        raise ReleaseIdentityError(f"App resources are missing: {resources}")

    if (resources / "OFFICESTRARuntime/node").exists() or (resources / "OFFICESTRARuntime/backend").exists():
        raise ReleaseIdentityError("Rust-only bundle contains a Node runtime or backend")

    files: list[Path] = []
    for relative in REQUIRED_PRESENT_FILES:
        path = resources / relative
        if not path.is_file() or path.is_symlink():
            raise ReleaseIdentityError(f"Required release file is missing: {path}")
    for relative in REQUIRED_FILES:
        path = resources / relative
        if not path.is_file() or path.is_symlink():
            raise ReleaseIdentityError(f"Required release file is missing: {path}")
        files.append(path)
    for relative in REQUIRED_TREES:
        files.extend(_regular_files(resources / relative))

    unique_files = sorted(
        set(files),
        key=lambda path: path.relative_to(resources).as_posix(),
    )
    return resources, unique_files


def calculate_release_id(app_bundle: Path) -> str:
    resources, files = release_input_files(app_bundle)
    identity = hashlib.sha256()
    identity.update(IDENTITY_FORMAT)
    for path in files:
        relative = path.relative_to(resources).as_posix().encode("utf-8")
        content = path.read_bytes()
        content_digest = hashlib.sha256(content).digest()
        identity.update(len(relative).to_bytes(8, "big"))
        identity.update(relative)
        identity.update(len(content).to_bytes(16, "big"))
        identity.update(content_digest)
    return identity.hexdigest()


def embedded_release_id(app_bundle: Path) -> str:
    info_plist = app_bundle / "Contents" / "Info.plist"
    if not info_plist.is_file():
        raise ReleaseIdentityError(f"App Info.plist is missing: {info_plist}")
    with info_plist.open("rb") as handle:
        info = plistlib.load(handle)
    release_id = info.get("OFFICESTRABackendReleaseID")
    if not isinstance(release_id, str) or not release_id.strip():
        raise ReleaseIdentityError(
            f"OFFICESTRABackendReleaseID is missing from {info_plist}"
        )
    return release_id


def verify_release_id(app_bundle: Path) -> str:
    calculated = calculate_release_id(app_bundle)
    embedded = embedded_release_id(app_bundle)
    if embedded != calculated:
        raise ReleaseIdentityError(
            "Embedded backend release ID does not match packaged runtime: "
            f"embedded={embedded} calculated={calculated}"
        )
    return calculated


def rust_runtime_records(path: Path, digest_pattern: str) -> dict[str, str]:
    """Read `<digest>  <name>` lines and require exactly RUST_RUNTIME_FILES."""
    records: dict[str, str] = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        match = re.fullmatch(rf"({digest_pattern})  (\S+)", line)
        if match is None or match.group(2) in records:
            raise ReleaseIdentityError(f"Bundled Rust record is malformed: {path}")
        records[match.group(2)] = match.group(1).lower()
    if set(records) != set(RUST_RUNTIME_FILES):
        raise ReleaseIdentityError(
            f"Bundled Rust record does not list the packaged files: {path}"
        )
    return records


def verify_rust_signature_metadata(app_bundle: Path) -> None:
    rust_root = (
        app_bundle
        / "Contents"
        / "Resources"
        / "OFFICESTRARuntime"
        / "rust"
    )
    rust_runtime_records(rust_root / "SHA256", r"[0-9a-f]{64}")
    expected_cdhashes = rust_runtime_records(rust_root / "CDHASH", r"[0-9a-fA-F]+")
    for name, expected_cdhash in expected_cdhashes.items():
        executable = rust_root / "bin" / name
        subprocess.run(
            ["/usr/bin/codesign", "--verify", "--strict", str(executable)],
            check=True,
            capture_output=True,
        )
        details = subprocess.run(
            ["/usr/bin/codesign", "-d", "--verbose=4", str(executable)],
            check=True,
            capture_output=True,
            text=True,
        )
        match = re.search(r"(?m)^CDHash=([0-9a-fA-F]+)$", details.stderr)
        if match is None or match.group(1).lower() != expected_cdhash:
            raise ReleaseIdentityError(
                f"Bundled Rust CDHash metadata does not match signature: {name}"
            )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("app_bundle", type=Path)
    parser.add_argument(
        "--verify",
        action="store_true",
        help="fail unless Info.plist contains the calculated identity",
    )
    parser.add_argument(
        "--verify-rust-signature", "--verify-node-signature",
        dest="verify_rust_signature",
        action="store_true",
        help="compare native executable signatures with packaged metadata (old flag is an alias)",
    )
    arguments = parser.parse_args()
    try:
        release_id = (
            verify_release_id(arguments.app_bundle)
            if arguments.verify
            else calculate_release_id(arguments.app_bundle)
        )
        if arguments.verify_rust_signature:
            verify_rust_signature_metadata(arguments.app_bundle)
    except (
        OSError,
        plistlib.InvalidFileException,
        ReleaseIdentityError,
        subprocess.CalledProcessError,
    ) as error:
        print(error, file=sys.stderr)
        return 1
    print(release_id)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
