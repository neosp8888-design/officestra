#!/bin/zsh
# 이 스크립트는 Swift 패키지를 빌드해 실행 가능한 macOS 앱 번들로 묶는다.

set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
DIST_DIR="$PROJECT_DIR/dist"
APP_BUNDLE="$DIST_DIR/OFFICESTRA.app"
NODE_ENTITLEMENTS="$PROJECT_DIR/scripts/node-runtime.entitlements"
BACKEND_RELEASE_ID_TOOL="$PROJECT_DIR/scripts/backend-release-id.py"
PACKAGED_EMBEDDING_SMOKE="$PROJECT_DIR/scripts/smoke-test-packaged-embedding.mjs"

cd "$PROJECT_DIR"
HOST_ARCHITECTURE="$(/usr/bin/uname -m)"
if [[ "$HOST_ARCHITECTURE" != "arm64" ]]; then
    print -u2 \
        "OFFICESTRA v1.3.2부터 Apple Silicon arm64에서만 빌드합니다. 현재: $HOST_ARCHITECTURE"
    exit 1
fi

NODE_EXECUTABLE="${OFFICESTRA_NODE_EXECUTABLE:-$(command -v node || true)}"
if [[ ! -x "$NODE_EXECUTABLE" ]]; then
    print -u2 "배포 앱에 포함할 Node 실행 파일을 찾을 수 없습니다."
    exit 1
fi
NODE_EXECUTABLE="$(realpath "$NODE_EXECUTABLE")"
# 앱에는 Node 실행 파일 하나만 넣으므로 Homebrew처럼 별도 dylib에 의존하는
# 빌드는 원래 설치 경로에서만 실행되고 번들 안에서는 시작할 수 없다. 긴 Swift
# 빌드에 들어가기 전에 이를 거부해 깨진 배포본과 불필요한 빌드 시간을 막는다.
EXTERNAL_NODE_DEPENDENCIES="$(
    /usr/bin/otool -L "$NODE_EXECUTABLE" \
        | /usr/bin/awk '
            NR > 1 && $1 !~ "^/System/Library/" && $1 !~ "^/usr/lib/" {
                print $1
            }
        '
)"
if [[ -n "$EXTERNAL_NODE_DEPENDENCIES" ]]; then
    print -u2 \
        "선택한 Node는 앱에 포함되지 않는 동적 라이브러리에 의존합니다. $NODE_EXECUTABLE"
    print -u2 "$EXTERNAL_NODE_DEPENDENCIES"
    print -u2 \
        "공식 독립형 Node 배포본을 OFFICESTRA_NODE_EXECUTABLE로 지정하세요."
    exit 1
fi
NODE_VERSION="$($NODE_EXECUTABLE -p 'process.versions.node')"
NODE_MAJOR="${NODE_VERSION%%.*}"
if (( NODE_MAJOR < 20 )); then
    print -u2 "OFFICESTRA 백엔드는 Node 20 이상이 필요합니다. 현재: $NODE_VERSION"
    exit 1
fi
NODE_PREFIX="$(cd "$(dirname "$NODE_EXECUTABLE")/.." && pwd)"
if [[ ! -f "$NODE_PREFIX/LICENSE" ]]; then
    print -u2 "Node 라이선스 파일을 찾을 수 없습니다. $NODE_PREFIX/LICENSE"
    exit 1
fi

# 병합으로 제거된 프레임워크 참조가 증분 산출물에 남지 않도록 배포 빌드를 깨끗하게 시작한다.
swift package clean
swift build -c release --product OfficeLLM
BIN_DIR="$(swift build -c release --show-bin-path)"
CORE_RESOURCE_BUNDLE="$BIN_DIR/OfficeLLM_OfficeCore.bundle"
GAME_RESOURCE_BUNDLE="$BIN_DIR/OfficeLLM_OfficeGame.bundle"

# SwiftPM 6.4 emits macOS-style resource bundles under Contents/Resources,
# while older toolchains emitted flat bundles. Normalize both layouts so the
# shipped app remains compatible with its existing flat resource lookup paths.
if [[ -f "$CORE_RESOURCE_BUNDLE/Info.plist" ]]; then
    CORE_RESOURCE_INFO="$CORE_RESOURCE_BUNDLE/Info.plist"
    CORE_RESOURCE_ROOT="$CORE_RESOURCE_BUNDLE"
elif [[ -f "$CORE_RESOURCE_BUNDLE/Contents/Info.plist" \
        && -d "$CORE_RESOURCE_BUNDLE/Contents/Resources" ]]; then
    CORE_RESOURCE_INFO="$CORE_RESOURCE_BUNDLE/Contents/Info.plist"
    CORE_RESOURCE_ROOT="$CORE_RESOURCE_BUNDLE/Contents/Resources"
else
    print -u2 "OfficeCore 리소스 번들 구조를 인식할 수 없습니다. $CORE_RESOURCE_BUNDLE"
    exit 1
fi

if [[ -f "$GAME_RESOURCE_BUNDLE/Info.plist" ]]; then
    GAME_RESOURCE_INFO="$GAME_RESOURCE_BUNDLE/Info.plist"
    GAME_RESOURCE_ROOT="$GAME_RESOURCE_BUNDLE"
elif [[ -f "$GAME_RESOURCE_BUNDLE/Contents/Info.plist" \
        && -d "$GAME_RESOURCE_BUNDLE/Contents/Resources" ]]; then
    GAME_RESOURCE_INFO="$GAME_RESOURCE_BUNDLE/Contents/Info.plist"
    GAME_RESOURCE_ROOT="$GAME_RESOURCE_BUNDLE/Contents/Resources"
else
    print -u2 "OfficeGame 리소스 번들 구조를 인식할 수 없습니다. $GAME_RESOURCE_BUNDLE"
    exit 1
fi

for required_path in \
    "$BIN_DIR/OfficeLLM" \
    "$CORE_RESOURCE_INFO" \
    "$CORE_RESOURCE_ROOT/characters.json" \
    "$CORE_RESOURCE_ROOT/office-retina-v1" \
    "$CORE_RESOURCE_ROOT/avatars" \
    "$CORE_RESOURCE_ROOT/profiles" \
    "$GAME_RESOURCE_INFO" \
    "$GAME_RESOURCE_ROOT/SystemMessages.en.json" \
    "$GAME_RESOURCE_ROOT/en.lproj/Localizable.strings" \
    "$GAME_RESOURCE_ROOT/ko.lproj/Localizable.strings"; do
    if [[ ! -e "$required_path" ]]; then
        print -u2 "필수 빌드 결과가 없습니다. $required_path"
        exit 1
    fi
done

if [[ ! -f "$NODE_ENTITLEMENTS" ]]; then
    print -u2 "Node 런타임 entitlements 파일이 없습니다. $NODE_ENTITLEMENTS"
    exit 1
fi
/usr/bin/plutil -lint "$NODE_ENTITLEMENTS" >/dev/null
if [[ ! -f "$BACKEND_RELEASE_ID_TOOL" ]]; then
    print -u2 "백엔드 릴리스 식별자 도구가 없습니다. $BACKEND_RELEASE_ID_TOOL"
    exit 1
fi
if [[ ! -f "$PACKAGED_EMBEDDING_SMOKE" ]]; then
    print -u2 \
        "패키지 임베딩 스모크 테스트가 없습니다. $PACKAGED_EMBEDDING_SMOKE"
    exit 1
fi

mkdir -p "$DIST_DIR"
STAGING_ROOT="$(mktemp -d "$DIST_DIR/.officestra-build.XXXXXX")"
STAGED_APP="$STAGING_ROOT/OFFICESTRA.app"
CONTENTS_DIR="$STAGED_APP/Contents"
MACOS_DIR="$CONTENTS_DIR/MacOS"
RESOURCES_DIR="$CONTENTS_DIR/Resources"
RUNTIME_DIR="$RESOURCES_DIR/OFFICESTRARuntime"
BACKEND_RUNTIME_DIR="$RUNTIME_DIR/backend"
NODE_RUNTIME_DIR="$RUNTIME_DIR/node"

cleanup() {
    if [[ -d "$STAGING_ROOT" ]]; then
        rm -rf "$STAGING_ROOT"
    fi
}

trap cleanup EXIT

mkdir -p "$MACOS_DIR" "$RESOURCES_DIR"
cp "$BIN_DIR/OfficeLLM" "$MACOS_DIR/OfficeLLM"
cp "$PROJECT_DIR/Resources/Info.plist" "$CONTENTS_DIR/Info.plist"

# SwiftUI의 고정 문구는 앱 주 번들의 Localizable.strings를 조회하므로,
# 패키지 리소스의 언어 디렉터리도 최종 앱 번들에 넣는다.
/usr/bin/rsync \
    -a \
    --delete \
    "$PROJECT_DIR/Sources/OfficeGame/Resources/" \
    "$RESOURCES_DIR/"

cp "$PROJECT_DIR/Resources/OFFICESTRA.icns" "$RESOURCES_DIR/OFFICESTRA.icns"

/usr/bin/rsync \
    -a \
    --delete \
    "$CORE_RESOURCE_ROOT/" \
    "$RESOURCES_DIR/OfficeLLM_OfficeCore.bundle/"
cp "$CORE_RESOURCE_INFO" \
    "$RESOURCES_DIR/OfficeLLM_OfficeCore.bundle/Info.plist"

/usr/bin/rsync \
    -a \
    --delete \
    "$GAME_RESOURCE_ROOT/" \
    "$RESOURCES_DIR/OfficeLLM_OfficeGame.bundle/"
cp "$GAME_RESOURCE_INFO" \
    "$RESOURCES_DIR/OfficeLLM_OfficeGame.bundle/Info.plist"

# 백엔드 코드, Node 런타임, Compose 설정과 production 의존성을 앱에
# 포함해 사용자 프로젝트와 OFFICESTRA 설치 소스의 경로를 분리한다.
mkdir -p \
    "$BACKEND_RUNTIME_DIR/src" \
    "$RUNTIME_DIR/database/migrations" \
    "$RUNTIME_DIR/infra" \
    "$NODE_RUNTIME_DIR/bin" \
    "$RUNTIME_DIR/licenses"
/usr/bin/rsync \
    -a \
    --delete \
    "$PROJECT_DIR/backend/src/" \
    "$BACKEND_RUNTIME_DIR/src/"
cp "$PROJECT_DIR/backend/package.json" "$BACKEND_RUNTIME_DIR/package.json"
cp "$PROJECT_DIR/backend/package-lock.json" \
    "$BACKEND_RUNTIME_DIR/package-lock.json"
/usr/bin/rsync \
    -a \
    --delete \
    "$PROJECT_DIR/database/migrations/" \
    "$RUNTIME_DIR/database/migrations/"
cp "$PROJECT_DIR/infra/compose.yaml" "$RUNTIME_DIR/infra/compose.yaml"

cp "$NODE_EXECUTABLE" "$NODE_RUNTIME_DIR/bin/node"
/usr/bin/shasum -a 256 "$NODE_RUNTIME_DIR/bin/node" \
    | /usr/bin/awk '{print $1}' \
    > "$NODE_RUNTIME_DIR/SHA256"
/usr/bin/printf '%s\n' "$NODE_VERSION" > "$NODE_RUNTIME_DIR/VERSION"
cp "$NODE_ENTITLEMENTS" "$NODE_RUNTIME_DIR/ENTITLEMENTS.plist"
cp "$NODE_PREFIX/LICENSE" "$RUNTIME_DIR/licenses/Node-LICENSE"
cp "$PROJECT_DIR/LICENSE" "$RUNTIME_DIR/licenses/OFFICESTRA-LICENSE"

canonical_architectures() {
    /usr/bin/lipo -archs "$1" \
        | /usr/bin/tr ' ' '\n' \
        | /usr/bin/sort \
        | /usr/bin/paste -sd ' ' -
}

APP_ARCHITECTURES="$(canonical_architectures "$MACOS_DIR/OfficeLLM")"
NODE_ARCHITECTURES="$(canonical_architectures "$NODE_RUNTIME_DIR/bin/node")"
if [[ -z "$APP_ARCHITECTURES" || "$APP_ARCHITECTURES" != "$NODE_ARCHITECTURES" ]]; then
    print -u2 "앱과 번들 Node 아키텍처가 일치하지 않습니다. app=$APP_ARCHITECTURES node=$NODE_ARCHITECTURES"
    exit 1
fi
if [[ "$APP_ARCHITECTURES" != "arm64" ]]; then
    print -u2 \
        "OFFICESTRA v1.3.2부터 Apple Silicon arm64만 지원합니다. 현재: $APP_ARCHITECTURES"
    exit 1
fi

npm \
    --prefix "$BACKEND_RUNTIME_DIR" \
    ci \
    --omit=dev \
    --ignore-scripts \
    --no-audit \
    --no-fund
# OFFICESTRA는 Apple Silicon 전용이다. onnxruntime-node가 함께 배포하는
# 타 운영체제 바이너리는 임시 앱 스테이징에서만 제거해 번들 크기와
# 불필요한 네이티브 코드를 줄인다.
ORT_NATIVE_ROOT="$BACKEND_RUNTIME_DIR/node_modules/onnxruntime-node/bin/napi-v6"
/bin/rm -rf \
    "$ORT_NATIVE_ROOT/linux" \
    "$ORT_NATIVE_ROOT/win32"
ORT_NATIVE_DIR="$ORT_NATIVE_ROOT/darwin/arm64"
ORT_DYLIB="$ORT_NATIVE_DIR/libonnxruntime.1.24.3.dylib"
ORT_BINDING="$ORT_NATIVE_DIR/onnxruntime_binding.node"
"$NODE_EXECUTABLE" \
    "$PROJECT_DIR/scripts/generate-third-party-notices.mjs" \
    "$BACKEND_RUNTIME_DIR/package-lock.json" \
    "$RUNTIME_DIR/licenses/THIRD-PARTY-NOTICES.md"

for packaged_runtime_path in \
    "$BACKEND_RUNTIME_DIR/src/server.mjs" \
    "$BACKEND_RUNTIME_DIR/src/officestra-result" \
    "$BACKEND_RUNTIME_DIR/src/officestra-terminal-hook" \
    "$BACKEND_RUNTIME_DIR/node_modules/pg/package.json" \
    "$BACKEND_RUNTIME_DIR/node_modules/ws/package.json" \
    "$BACKEND_RUNTIME_DIR/node_modules/@slack/bolt/package.json" \
    "$BACKEND_RUNTIME_DIR/node_modules/@huggingface/tokenizers/package.json" \
    "$BACKEND_RUNTIME_DIR/node_modules/onnxruntime-node/package.json" \
    "$BACKEND_RUNTIME_DIR/node_modules/playwright-core/package.json" \
    "$BACKEND_RUNTIME_DIR/src/playwright-driver-1.57.0/package/cli.js" \
    "$ORT_DYLIB" \
    "$ORT_BINDING" \
    "$RUNTIME_DIR/database/migrations/001_initial.sql" \
    "$RUNTIME_DIR/infra/compose.yaml" \
    "$NODE_RUNTIME_DIR/bin/node" \
    "$NODE_RUNTIME_DIR/SHA256" \
    "$NODE_RUNTIME_DIR/VERSION" \
    "$NODE_RUNTIME_DIR/ENTITLEMENTS.plist" \
    "$RUNTIME_DIR/licenses/Node-LICENSE" \
    "$RUNTIME_DIR/licenses/THIRD-PARTY-NOTICES.md"; do
    if [[ ! -e "$packaged_runtime_path" ]]; then
        print -u2 "필수 백엔드 런타임이 없습니다. $packaged_runtime_path"
        exit 1
    fi
done

if [[ ! -x "$BACKEND_RUNTIME_DIR/src/officestra-result" ]]; then
    print -u2 "응답 메타데이터 도구에 실행 권한이 없습니다."
    exit 1
fi
if [[ ! -x "$BACKEND_RUNTIME_DIR/src/officestra-terminal-hook" ]]; then
    print -u2 "터미널 이벤트 훅에 실행 권한이 없습니다."
    exit 1
fi

RUNTIME_CONFIG="$RESOURCES_DIR/OfficeLLM_OfficeCore.bundle/characters.json"
RUNTIME_WORKDIR="${OFFICESTRA_WORKDIR:-/Users/your-name/Projects}"

OFFICESTRA_RUNTIME_CONFIG="$RUNTIME_CONFIG" \
OFFICESTRA_CONFIGURED_WORKDIR="$RUNTIME_WORKDIR" \
/usr/bin/env node <<'NODE'
const fs = require("node:fs");
const nodePath = require("node:path");

const configPath = process.env.OFFICESTRA_RUNTIME_CONFIG;
const configuration = JSON.parse(fs.readFileSync(configPath, "utf8"));
const configuredWorkdir = process.env.OFFICESTRA_CONFIGURED_WORKDIR?.trim();

configuration.workdir = configuredWorkdir;

if (
  !nodePath.isAbsolute(configuration.workdir)
) {
  throw new Error(`업무 폴더가 절대 경로가 아닙니다. ${configuration.workdir}`);
}

fs.writeFileSync(configPath, `${JSON.stringify(configuration, null, 2)}\n`);
NODE

chmod 755 "$MACOS_DIR/OfficeLLM"
chmod 755 "$NODE_RUNTIME_DIR/bin/node"
# SwiftPM release 산출물의 DWARF에는 빌드 머신의 절대 소스 경로가 남으므로
# 배포 번들에 넣기 전에 디버그 심볼을 제거한다.
/usr/bin/strip -S "$MACOS_DIR/OfficeLLM"
CODESIGN_IDENTITY="${OFFICESTRA_CODESIGN_IDENTITY:--}"
if [[ "$CODESIGN_IDENTITY" == "-" ]]; then
    codesign \
        --force \
        --entitlements "$NODE_ENTITLEMENTS" \
        --sign - \
        "$NODE_RUNTIME_DIR/bin/node"
else
    codesign \
        --force \
        --entitlements "$NODE_ENTITLEMENTS" \
        --options runtime \
        --timestamp \
        --sign "$CODESIGN_IDENTITY" \
        "$NODE_RUNTIME_DIR/bin/node"
fi

if [[ "$CODESIGN_IDENTITY" == "-" ]]; then
    codesign --force --sign - "$ORT_DYLIB"
    codesign --force --sign - "$ORT_BINDING"
else
    codesign \
        --force \
        --options runtime \
        --timestamp \
        --sign "$CODESIGN_IDENTITY" \
        "$ORT_DYLIB"
    codesign \
        --force \
        --options runtime \
        --timestamp \
        --sign "$CODESIGN_IDENTITY" \
        "$ORT_BINDING"
fi

# Developer ID 재서명에서 기존 Node 배포본의 광범위한 entitlement를 무작정
# 보존하지 않는다. V8 JIT에 필요한 두 권한만 남았는지 서명 결과를 정확히
# 비교하고, 실제 실행과 동적 코드 생성까지 확인한다.
SIGNED_NODE_ENTITLEMENTS="$STAGING_ROOT/signed-node.entitlements"
codesign \
    -d \
    --entitlements :- \
    "$NODE_RUNTIME_DIR/bin/node" \
    > "$SIGNED_NODE_ENTITLEMENTS" \
    2>/dev/null
OFFICESTRA_EXPECTED_NODE_ENTITLEMENTS="$NODE_ENTITLEMENTS" \
OFFICESTRA_SIGNED_NODE_ENTITLEMENTS="$SIGNED_NODE_ENTITLEMENTS" \
/usr/bin/python3 <<'PY'
import os
import plistlib
import sys

expected_path = os.environ["OFFICESTRA_EXPECTED_NODE_ENTITLEMENTS"]
signed_path = os.environ["OFFICESTRA_SIGNED_NODE_ENTITLEMENTS"]
with open(expected_path, "rb") as handle:
    expected = plistlib.load(handle)
with open(signed_path, "rb") as handle:
    signed = plistlib.load(handle)

if signed != expected:
    print(
        "서명된 Node entitlement가 최소 배포 정책과 다릅니다. "
        f"expected={expected!r} actual={signed!r}",
        file=sys.stderr,
    )
    raise SystemExit(1)
PY

"$NODE_RUNTIME_DIR/bin/node" --version >/dev/null
PACKAGED_NODE_VERSION="$($NODE_RUNTIME_DIR/bin/node -p 'process.versions.node')"
if [[ "$PACKAGED_NODE_VERSION" != "$(<"$NODE_RUNTIME_DIR/VERSION")" ]]; then
    print -u2 "번들 Node 버전 기록이 실행 파일과 일치하지 않습니다."
    exit 1
fi
NODE_CDHASH="$(
    codesign -d --verbose=4 "$NODE_RUNTIME_DIR/bin/node" 2>&1 \
        | /usr/bin/sed -n 's/^CDHash=//p' \
        | /usr/bin/head -n 1
)"
if [[ -z "$NODE_CDHASH" ]]; then
    print -u2 "번들 Node CDHash를 읽지 못했습니다."
    exit 1
fi
/usr/bin/printf '%s\n' "$NODE_CDHASH" > "$NODE_RUNTIME_DIR/CDHASH"
"$NODE_RUNTIME_DIR/bin/node" --check "$BACKEND_RUNTIME_DIR/src/server.mjs"
"$NODE_RUNTIME_DIR/bin/node" \
    "$BACKEND_RUNTIME_DIR/src/officestra-result" \
    --help >/dev/null
"$NODE_RUNTIME_DIR/bin/node" -e '
const increment = new Function("value", "return value + 1");
let value = 0;
for (let index = 0; index < 250000; index += 1) {
  value = increment(value);
}
if (value !== 250000) {
  throw new Error(`Node JIT smoke test failed: ${value}`);
}
'
"$NODE_RUNTIME_DIR/bin/node" \
    "$PACKAGED_EMBEDDING_SMOKE" \
    "$BACKEND_RUNTIME_DIR"

# 서명된 Node를 포함해 최종 번들에서 백엔드가 실제로 읽는 파일만 해시한다.
# Info.plist 자체는 순환 입력이므로 제외하고 계산한 값을 주입한 뒤 재계산한다.
BACKEND_RELEASE_ID="$(
    /usr/bin/python3 "$BACKEND_RELEASE_ID_TOOL" "$STAGED_APP"
)"
if [[ -z "$BACKEND_RELEASE_ID" ]]; then
    print -u2 "백엔드 릴리스 식별자를 만들지 못했습니다."
    exit 1
fi
/usr/libexec/PlistBuddy \
    -c "Set :OFFICESTRABackendReleaseID $BACKEND_RELEASE_ID" \
    "$CONTENTS_DIR/Info.plist"
/usr/bin/python3 \
    "$BACKEND_RELEASE_ID_TOOL" \
    --verify \
    --verify-node-signature \
    "$STAGED_APP" >/dev/null

if [[ "$CODESIGN_IDENTITY" == "-" ]]; then
    codesign --force --sign - "$MACOS_DIR/OfficeLLM"
    codesign --force --sign - "$STAGED_APP"
else
    codesign \
        --force \
        --options runtime \
        --timestamp \
        --sign "$CODESIGN_IDENTITY" \
        "$MACOS_DIR/OfficeLLM"
    codesign \
        --force \
        --options runtime \
        --timestamp \
        --sign "$CODESIGN_IDENTITY" \
        "$STAGED_APP"
fi
codesign --verify --deep --strict --verbose=2 "$STAGED_APP"
/usr/bin/python3 \
    "$BACKEND_RELEASE_ID_TOOL" \
    --verify \
    --verify-node-signature \
    "$STAGED_APP" >/dev/null

if [[ -e "$APP_BUNDLE" ]]; then
    /usr/bin/swift -e '
        import Darwin

        let installedApp = CommandLine.arguments[1]
        let stagedApp = CommandLine.arguments[2]
        guard renamex_np(
            installedApp,
            stagedApp,
            UInt32(RENAME_SWAP)
        ) == 0 else {
            perror("OFFICESTRA.app 원자적 교체")
            exit(1)
        }
    ' "$APP_BUNDLE" "$STAGED_APP"
else
    mv "$STAGED_APP" "$APP_BUNDLE"
fi

echo "$APP_BUNDLE"
