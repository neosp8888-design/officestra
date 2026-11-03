#!/bin/zsh
# 앱 또는 ZIP 안의 앱을 격리된 첫 실행 상태로 실제 기동한다.

set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ARTIFACT="${1:-}"
SMOKE_SECONDS="${OFFICESTRA_APP_SMOKE_SECONDS:-8}"
TEMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/officestra-app-smoke.XXXXXX")"
APP_PID=""

cleanup() {
    if [[ -n "$APP_PID" ]] && kill -0 "$APP_PID" 2>/dev/null; then
        kill "$APP_PID" 2>/dev/null || true
        wait "$APP_PID" 2>/dev/null || true
    fi
    rm -rf "$TEMP_ROOT"
}
trap cleanup EXIT

if [[ -z "$ARTIFACT" || ! -e "$ARTIFACT" ]]; then
    print -u2 "스모크 테스트할 앱 또는 ZIP이 없습니다. $ARTIFACT"
    exit 1
fi

case "$ARTIFACT" in
    *.app)
        APP_PATH="$ARTIFACT"
        ;;
    *.zip)
        EXTRACT_DIR="$TEMP_ROOT/extracted"
        mkdir -p "$EXTRACT_DIR"
        /usr/bin/ditto -x -k "$ARTIFACT" "$EXTRACT_DIR"
        APP_PATH="$EXTRACT_DIR/OFFICESTRA.app"
        ;;
    *)
        print -u2 "스모크 테스트 형식은 .app 또는 .zip이어야 합니다. $ARTIFACT"
        exit 1
        ;;
esac

EXECUTABLE="$APP_PATH/Contents/MacOS/OfficeLLM"
if [[ ! -x "$EXECUTABLE" ]]; then
    print -u2 "실행 가능한 OFFICESTRA 앱을 찾지 못했습니다. $EXECUTABLE"
    exit 1
fi

/usr/bin/codesign --verify --deep --strict --verbose=2 "$APP_PATH"
/usr/bin/python3 \
    "$PROJECT_DIR/scripts/backend-release-id.py" \
    --verify \
    --verify-rust-signature \
    "$APP_PATH" >/dev/null

ISOLATED_HOME="$TEMP_ROOT/home"
ISOLATED_TMP="$TEMP_ROOT/tmp"
APP_LOG="$TEMP_ROOT/app.log"
mkdir -p "$ISOLATED_HOME" "$ISOLATED_TMP"

HOME="$ISOLATED_HOME" \
CFFIXED_USER_HOME="$ISOLATED_HOME" \
TMPDIR="$ISOLATED_TMP/" \
    "$EXECUTABLE" >"$APP_LOG" 2>&1 &
APP_PID="$!"
sleep "$SMOKE_SECONDS"

if ! kill -0 "$APP_PID" 2>/dev/null; then
    print -u2 "OFFICESTRA 앱이 스모크 시간 전에 종료됐습니다."
    print -u2 -- "$(<"$APP_LOG")"
    wait "$APP_PID" 2>/dev/null || true
    APP_PID=""
    exit 1
fi

if /usr/bin/grep -Eiq \
    'fatal error:|uncaught exception|abort trap|segmentation fault|trace/breakpoint trap|dyld:.*library not loaded' \
    "$APP_LOG"; then
    print -u2 "OFFICESTRA 앱 로그에서 치명적 오류를 발견했습니다."
    print -u2 -- "$(<"$APP_LOG")"
    exit 1
fi

print "OFFICESTRA release smoke passed: $ARTIFACT"
