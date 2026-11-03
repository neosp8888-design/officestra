#!/usr/bin/env python3
"""Reject credentials and developer-local paths from release inputs."""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path


SENSITIVE_NAMES = re.compile(
    r"(?:^|/)(?:\.env(?!\.(?:example|sample|template)$)(?:\.[^/]*)?|"
    r"id_(?:rsa|ed25519)|credentials(?:\.[^/]*)?|"
    r"secrets?(?:\.[^/]*)?|[^/]+\.(?:pem|key|p12|pfx|mobileprovision))$",
    re.IGNORECASE,
)
SECRET_PATTERNS = {
    "private key": re.compile(
        rb"-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----"
    ),
    "AWS access key": re.compile(rb"AKIA[0-9A-Z]{16}"),
    "GitHub token": re.compile(rb"gh[pousr]_[A-Za-z0-9]{30,}"),
    "Slack token": re.compile(rb"xox[baprs]-[A-Za-z0-9-]{20,}"),
    "OpenAI or Anthropic key": re.compile(rb"sk-[A-Za-z0-9_-]{24,}"),
    "Google API key": re.compile(rb"AIza[A-Za-z0-9_-]{30,}"),
}
LOCAL_PATH_PATTERN = re.compile(rb"/Users/([^/\x00\s]+)(?:/|\x00)")


def tracked_paths() -> list[Path]:
    tracked = subprocess.check_output(["git", "ls-files", "-z"]).split(b"\0")
    return [Path(value.decode()) for value in tracked if value]


def regular_files(root: Path):
    if root.is_file():
        yield root
    elif root.is_dir():
        yield from (path for path in root.rglob("*") if path.is_file())


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--tracked-only",
        action="store_true",
        help="scan source inputs before an app bundle exists",
    )
    parser.add_argument(
        "--require-app",
        action="store_true",
        help="fail unless dist/OFFICESTRA.app exists and is scanned",
    )
    arguments = parser.parse_args()
    if arguments.tracked_only and arguments.require_app:
        parser.error("--tracked-only and --require-app cannot be combined")

    tracked = tracked_paths()
    bad_names = [
        str(path) for path in tracked if SENSITIVE_NAMES.search(path.as_posix())
    ]
    bad_contents: list[str] = []
    for path in tracked:
        try:
            content = path.read_bytes()
        except (IsADirectoryError, OSError):
            continue
        for label, pattern in SECRET_PATTERNS.items():
            if pattern.search(content):
                bad_contents.append(f"{path}: {label}")

    app = Path("dist/OFFICESTRA.app")
    if arguments.require_app and not app.is_dir():
        print(f"Required app bundle is missing: {app}", file=sys.stderr)
        return 1
    scan_app = not arguments.tracked_only and app.is_dir()
    bundle_sensitive_names = (
        [
            str(path)
            for path in app.rglob("*")
            if path.is_file() and SENSITIVE_NAMES.search(path.as_posix())
        ]
        if scan_app
        else []
    )

    scan_roots = [
        Path("Sources/OfficeCore/Resources/characters.json"),
        Path("Sources/OfficeGame/Resources"),
        Path("backend/src"),
        Path("database"),
        Path("infra"),
        Path("Resources"),
        Path("scripts/build-app.sh"),
    ]
    if scan_app:
        scan_roots.append(app)
    bad_local_paths: list[str] = []
    seen: set[Path] = set()
    for root in scan_roots:
        for path in regular_files(root):
            if path in seen:
                continue
            seen.add(path)
            relative = path.as_posix()
            # Third-party prebuilt binaries keep their vendors' build paths. The
            # Rust ONNX Runtime library is a copy of the onnxruntime-node one.
            if (
                "/OFFICESTRARuntime/node/" in relative
                or "/node_modules/" in relative
                or relative.endswith("/OFFICESTRARuntime/rust/bin/libonnxruntime.dylib")
            ):
                continue
            try:
                content = path.read_bytes()
            except OSError:
                continue
            users = {
                match.group(1).decode("utf-8", errors="replace")
                for match in LOCAL_PATH_PATTERN.finditer(content)
            }
            users.discard("your-name")
            if users:
                bad_local_paths.append(
                    f"{path}: {', '.join(sorted(users))}"
                )

    failures: list[str] = []
    if bad_names:
        failures.append(
            "Sensitive tracked filenames:\n  " + "\n  ".join(sorted(bad_names))
        )
    if bad_contents:
        failures.append(
            "Secret-like tracked contents:\n  "
            + "\n  ".join(sorted(bad_contents))
        )
    if bundle_sensitive_names:
        failures.append(
            "Sensitive filenames in app bundle:\n  "
            + "\n  ".join(sorted(bundle_sensitive_names))
        )
    if bad_local_paths:
        failures.append(
            "Developer user paths in shipping inputs or app bundle:\n  "
            + "\n  ".join(sorted(bad_local_paths))
        )
    if failures:
        print("\n\n".join(failures), file=sys.stderr)
        return 1
    print("Release inputs contain no known credentials or developer-local paths.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
