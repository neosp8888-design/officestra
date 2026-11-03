#!/bin/bash
# Node 없는 앱/ZIP의 격리 DB·첫 업무·네이티브 근거 제출 검증.
set -euo pipefail
PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
if [[ $# -ne 1 ]]; then
    echo "검증할 OFFICESTRA.app 또는 ZIP 경로가 필요합니다." >&2
    exit 1
fi
exec python3 "$PROJECT_DIR/scripts/test-rust-preview-e2e.py" "$1"
