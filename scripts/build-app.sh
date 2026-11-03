#!/bin/zsh
# Rust 전용 앱 빌드. Node 백엔드·Node 런타임은 배포하지 않는다.
set -euo pipefail
PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
exec python3 "$PROJECT_DIR/scripts/build-rust-app.py" "$@"
