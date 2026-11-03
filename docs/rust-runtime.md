# Rust 운영 런타임과 전환 확인

## 2026-11-04 저장소 통합

- Rust 원본은 이 저장소의 `backend-rs/`로 옮겼으며 앱과 같은 `officestra` 원격으로 관리한다.
- 이전 `/Users/neo/office-rs`는 `backend-rs/`를 가리키는 호환 링크다. 이전 독립 Git 이력(비용 수정 `0d1fa16` 포함)은 `~/Library/Application Support/OFFICESTRA/Backups/repositories/office-rs-20261104.git`에 보관했다.
- 빌드·개발 실행·테스트는 `backend-rs/`를 사용한다. `scripts/start-backend.sh`도 Rust 실행 파일만 실행한다.
- `backend/` Node 코드는 이전 비교·이력 자료다. 앱에는 Rust 실행 파일만 포함한다. 외부 AI CLI 도구 자체의 Node 사용은 별개다.
- GPT 가격표는 열 이름으로 해석하고, 구형 파서 캐시는 버전 검사로 폐기한다. 표시 비용 복구 도구는 `backend-rs/examples/repair_codex_display_costs.rs`다. 실제 청구나 토큰 사용 기록은 변경하지 않는다.
- `backend-rs/target/`, 개인 환경 파일, 로그, 캡처·생성 이미지는 커밋에서 제외한다.
- 통합 경로에서 Rust 전체 723개(실패·건너뜀 0), 앱 실행·설정·보드 관련 Swift 56개, 릴리스 ID 7개·설치 도구 6개 테스트를 통과했다. Swift는 빌드 캐시의 `missing creator for mutated node` 경고 1건이 있었고 테스트는 모두 통과했다.
- 통합 경로로 `dist/OFFICESTRA.app` 재빌드 및 서명·네이티브 결과 도구·1024차원 임베딩 검증을 완료했다(릴리스 ID `42887c63`). 실행 중인 4317과 운영 앱은 재시작하지 않았다.

2026-11-03 R25/R26 정리. 이 문서는 구현·자동 검증과 사용자 확인을 구분한다.

## 현재 제공하는 것

- 운영 4317은 이미 Rust 백엔드다. 이번 작업에서 운영 백엔드를 재시작하지 않았다.
- 운영·빌드·실행 경로는 **`dist/OFFICESTRA.app` 하나**다. 2026-11-03 사용자가 "그냥 빌드해 내가 재시작하면 되니"라고 지시하여 이 경로에 직접 빌드한다. 종료를 기다리는 단발 교체 도구는 폐기했으며, 파일 빌드와 실제 프로세스 재시작은 구분한다.
- 현재 경로에 새 앱 빌드·서명·릴리스 ID 검증을 완료했고, 생성했던 `dist/r26` 앱 두 개와 해당 폴더는 정리했다. 빌드 직후 운영 프로세스는 이전 `7e1f2304…`를 유지했으며 새 `ef10b848…`의 프로세스 적용은 사용자 재시작 뒤 확인한다. 원문 빌드 로그는 `/tmp/officestra-r26-single-path-build.log`다.
- **사용자 재시작 후 적용 확인 완료:** 실제 PID `89816`, 실행 파일 `dist/OFFICESTRA.app/Contents/Resources/OFFICESTRARuntime/rust/bin/officestra-backend`, releaseID `ef10b848d1ace95e7d6b3050ade5d64cec0664ba6ae66770513eafac39edd8ec`가 번들 기대값과 일치한다. health·DB·업무 접수 정상. 추가 재시작이나 다른 경로 실행을 요구하지 않는다.
- 새 앱에는 `OFFICESTRARuntime/node`와 `OFFICESTRARuntime/backend`가 없다.
- Rust 실행 파일은 backend, result, terminal-hook, voice-follow, telegram 다섯 개다.
- `officestra-result`는 서버의 검증·잠금·메타데이터 파일 쓰기 함수를 재사용한다. 직원의 근거 제출을 Node로 실행하지 않는다.
- Rust가 없거나 실행 불가능하면 설치 오류다. Node 서버로 자동 전환하지 않는다.
- Antigravity CLI의 Playwright 1.57 어댑터는 제3자 JavaScript 드라이버다. 앱은 드라이버만 포함하고, 필요할 때 사용자 CLI 환경의 Node 경로를 넘긴다. **외부 CLI까지 포함한 Node 의존성 0을 의미하지 않는다.** Node가 없어도 Rust 서버와 네이티브 근거 제출은 실행된다.

## 사용자 버튼으로 적용하기

저장소 지침에 따라 4317 중지·시작은 사용자가 앱 버튼으로 수행한다. 사용 중인 앱이나 launchd 작업을 외부 명령으로 교체하지 않는다.

1. 현재 경로의 빌드 완료 보고를 기다린다. 별도 교체 알림이나 자동 대기는 없다.
2. 진행 중인 직원 업무와 답변이 끝난 뒤 기존 앱의 백엔드 중지 버튼을 누르고 앱을 종료한다.
3. 기존 OFFICESTRA 아이콘/경로로 앱을 다시 열고 백엔드 시작 버튼을 누른다. 빌드 도구는 앱·백엔드를 중지하거나 시작하지 않는다. 연결 주소나 인증 정보는 보고에 복사하지 않는다.
4. `/health`의 `ok`·DB 상태·releaseID와 실제 4317 실행 경로가 `dist/OFFICESTRA.app`의 Rust 파일인지 대조한다. 기대 releaseID는 `ef10b848d1ace95e7d6b3050ade5d64cec0664ba6ae66770513eafac39edd8ec`이며 `python3 scripts/backend-release-id.py --verify --verify-rust-signature dist/OFFICESTRA.app`으로 다시 확인한다.
5. 앱에서 직원 업무와 근거 표시, 업무 보드, 사용량 표시, 롤플레이, 터미널 세션을 확인한다. 4090 실제 생성·모델 기동과 Telegram 실제 송수신은 사용자 확인 전 자동 수행하지 않는다.

자동 테스트 통과는 사용자 확인을 대신하지 않는다. 이번에는 사용자가 직접 재시작했다고 보고했고 새 앱의 실행 경로·릴리스·DB·주요 API를 실제로 대조했다. 이를 운영 전환 확인으로 기록해 R25/R26을 마감한다. 아래 미실행 범위까지 사용자가 승인하거나 직접 시험했다고 기록하지 않으며, 릴리스 지시로도 간주하지 않는다.

## 이번 확인 결과와 제한

- Rust 전체 721개 통과, 실패·무시 0. 격리 PostgreSQL 환경을 제공한 실행이며 실제 프로세스를 띄우는 네이티브 결과 CLI 시험도 포함한다.
- 관련 Swift 실행기·설치 테스트 52개, 릴리스 ID 회귀 테스트 7개, window-nudge 단위 테스트 29개 통과.
- 단일 경로 설치 도구 6개 테스트 통과: 기본 실행 중 차단·백업 중 재실행 차단·정상 교체·실패 시 복원·복원까지 실패한 경우 원본 보존·명시적 빌드 허용 시 프로세스 조작 없이 파일만 교체.
- 서명·릴리스 ID·Node 미포함 검사, 비밀정보·개발자 경로 검사, 네이티브 메타데이터·1024차원 임베딩 스모크 통과.
- 별도 DB에서 첫 업무 접수→완료→근거 저장, CLI 경로 동기화, 보드 생성·수정·활동 쓰기 통과. 시험 서버·컨테이너·볼륨 정리 확인.
- Telegram 설치기는 임시 HOME에서 파일 동일성·서명·직접 네이티브 명령·백업을 확인했다. 실제 서비스의 바이너리·plist도 백업 후 준비했지만 프로세스를 재시작하거나 메시지를 보내지는 않았다.
- 운영 API의 characters·work-board·active-sessions·local-profiles·usage-summary는 HTTP 200이다. 최초 점검에서 Claude 한도 API의 HTTP 401·stale을 확인했으나, 후속 재시작 점검에서는 Codex·Claude·Antigravity 모두 오류 없음·stale=false였다. **401은 후속 조회에서 재현되지 않았다.** 새 로그인·토큰 교체·인증 코드 수정은 하지 않았으며 원인이 규명됐다고 주장하지 않는다.
- Swift 전체·사용자 화면 흐름·실제 Telegram 송수신·4090 모델 기동은 이번 자동 검증 범위 밖이다. Swift 빌드는 기존 SwiftTerm 관련 경고를 출력했으나 성공했다.
- 재시작 후 설치된 네이티브 결과 CLI로 임시 메타데이터의 저장·조회 왕복을 확인했다. 운영 턴의 제출 파일을 위조하거나 임시 결과를 실제 제출로 합산하지 않았다. `/api/roleplay/models` HTTP 200·모델 14개, `/api/terminal-sessions` HTTP 200·열린 세션 0개를 확인했다. 실제 대화 메시지를 보내거나 터미널을 새로 실행한 시험은 아니다.
- 실행 앱의 프로세스는 확인했으나 스크린샷은 macOS Screen Recording 권한이 없어 실패했다. 화면을 보았거나 전체 UI 상호작용을 검증했다고 주장하지 않는다. Telegram은 기존 Rust 프로세스가 유지 중이고 오류 로그의 마지막 수정은 이번 재시작 이전이다. 이번에 실제 메시지를 보내거나 외부 모델을 기동하지 않았다.

## 되돌리기

백엔드 중지 → 앱 종료 → `~/Library/Application Support/OFFICESTRA/Backups/apps/OFFICESTRA-시각.zip`을 별도 임시 폴더에 해제 → 확인한 이전 앱을 같은 `dist/OFFICESTRA.app` 경로에 복원 → 기존 아이콘으로 앱 실행·백엔드 시작 순서다. 운영 폴더 옆에 복구용 `.app`을 나란히 두지 않는다. 기존 데이터베이스를 지우거나 복원하지 않는다. 이전 앱 역시 Rust 운영판이며, Node 서버 자동 우회를 복구 절차로 사용하지 않는다.

빌더는 기본적으로 실행 중 덮어쓰기를 거부한다. 사용자가 현재 경로의 빌드를 명시적으로 지시한 이번에는 `OFFICESTRA_REPLACE_RUNNING_APP=1`로 번들 파일 교체만 허용했다. 이전 앱은 외부 ZIP으로 보존하고, 새 번들을 완성·검증한 뒤 경로를 교체한다. 프로세스는 자동으로 재시작하지 않으며 사용자가 앱 버튼으로 수행한다.

## Telegram 파일 설치와 복구

`python3 scripts/install-telegram-rust.py dist/OFFICESTRA.app`은 서명을 확인한 Rust Telegram 실행 파일을 `~/Library/Application Support/OFFICESTRA/bin`에 원자적으로 복사하고, launchd 설정을 그 안정 경로로 맞춘다. 실행 중인 서비스는 재시작하지 않는다. 기존 서비스가 계속 사용하던 파일과 plist는 각각 `bin/previous/`와 `.plist.r26-backup`에 보존된다. 이번 설치는 경로 통일 전 검증 후보에서 이미 수행했다.

다음 명시적 서비스 재시작이나 로그인부터 새 파일을 사용한다. 되돌릴 때는 서비스를 정상 중지한 뒤 위 백업 파일과 plist를 복원한다. 두 폴러를 동시에 띄우거나 인증·상태 파일을 초기화하지 않는다. 설치 검증은 임시 HOME에서 수행하며 실제 Telegram 호출은 하지 않는다.

## 개발·검증 명령

```sh
# DB 통합 테스트는 준비된 격리 DB 환경에서 실행한다.
PATH="$HOME/.cargo/bin:$PATH" python3 scripts/office-test.py --cwd backend-rs --kind cargo -- cargo test --offline
python3 scripts/office-test.py --kind swift -- swift test --filter 'OfficeBackendLaunchCommandsTests|OfficeSetupAssistantTests'
python3 scripts/test-backend-release-id.py
python3 scripts/test-rust-app-install.py
zsh scripts/build-app.sh
# 사용자가 실행 중인 현재 경로 빌드를 명시적으로 지시한 경우만:
OFFICESTRA_REPLACE_RUNNING_APP=1 zsh scripts/build-app.sh
python3 scripts/verify-release-safety.py --require-app --app dist/OFFICESTRA.app
bash scripts/test-packaged-community-preview-e2e.sh dist/OFFICESTRA.app
```

격리 E2E는 별도 Docker 프로젝트·빈 DB·4319를 사용한다. 가짜 CLI로 업무 실행·완료·네이티브 근거 저장과 보드 생성·수정·활동 쓰기를 확인한 뒤 서버·컨테이너·볼륨을 정리한다. 운영 4317·DB·실제 직원 CLI·Slack·Telegram을 쓰지 않는다.

빌드는 Rust·Swift·Python과 캐시된 ONNX Runtime dylib를 사용한다. `OFFICESTRA_ORT_DYLIB`로 검증된 외부 빌드 입력을 지정할 수 있다. 기본값은 기존 `backend/node_modules/onnxruntime-node` 캐시이며 npm이나 Node를 빌드 과정에서 실행하지 않는다. ONNX Runtime MIT 및 Rust crate 라이선스 원문을 번들에 포함한다.

`backend/` Node 소스·테스트와 `verify-local-codex.mjs`, `smoke-test-packaged-rust-embedding.mjs` 등 이전 비교 도구는 이력·회귀 비교용으로 보존했다. 새 앱 빌드나 운영 실행 경로에서는 사용하지 않는다. 이전 V3 앱의 릴리스 ID는 해당 버전의 검증 도구로 확인해야 한다. 현재 V4 도구는 Node가 든 앱을 거부한다.

## 이번 수정 파일

`office`:

- `AGENTS.md`
- `Sources/OfficeGame/OfficeBackendController.swift`
- `Sources/OfficeGame/OfficeSetupAssistant.swift`
- `Tests/OfficeCoreTests/OfficeBackendLaunchCommandsTests.swift`
- `Tests/OfficeCoreTests/OfficeSetupAssistantTests.swift`
- `scripts/backend-release-id.py`
- `scripts/build-app.sh`
- `scripts/build-rust-app.py`
- `scripts/com.neo.officestra-telegram.plist`
- `scripts/fixtures/fake-codex.sh`
- `scripts/install-telegram-rust.py`
- `scripts/rust_app_install.py`
- `scripts/smoke-test-macos-release.sh`
- `scripts/smoke-test-rust-runtime.py`
- `scripts/test-backend-release-id.py`
- `scripts/test-community-preview-e2e.sh`
- `scripts/test-packaged-community-preview-e2e.sh`
- `scripts/test-rust-preview-e2e.py`
- `scripts/test-rust-app-install.py`
- `scripts/verify-release-safety.py`
- `Vendor/ONNXRuntime/LICENSE`
- `docs/rust-runtime.md`

`backend-rs` (전환 작업 당시에는 독립 `office-rs`):

- `src/bin/officestra-result.rs`
- `src/config.rs`
- `src/lib.rs`
- `tests/result_cli.rs`

추가 운영 파일은 Telegram 안정 경로 실행 파일과 launchd plist 두 개이며, 기존 파일의 복구 사본도 만들었다. 커밋·푸시는 이번 작업에 포함하지 않았다.
