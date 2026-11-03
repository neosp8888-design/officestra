#!/bin/bash
# Rust 소스 빌드의 격리 DB·첫 업무 검증.
set -euo pipefail
PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
exec python3 "$PROJECT_DIR/scripts/test-rust-preview-e2e.py" "$@"
