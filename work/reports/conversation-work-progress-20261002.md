# 대화 작업 현황 표시

대화 상단에 선택한 직원의 현재 동작과 공개 진행 설명을 짧게 보여 준다.
GUI와 터미널에 같은 컴포넌트를 사용하며, 분할 모드에서는 포커스된 직원을 따른다.
오른쪽 목록 아이콘을 누르면 최근 6개 작업 기록, 명시된 계획의 다음 항목,
마지막 기록 시각을 볼 수 있다. 추가 AI 호출은 없다.

## 표시 기준

- 자료 검색, 내용 확인, 파일 수정, 검증, 빌드, 외부 조회, 협업을 알려진 명령·도구로 구분한다.
- 알 수 없는 명령은 일반 작업으로 표시한다. 인자·명령 출력의 단어로 작업 의도를 추측하지 않는다.
- 명령 종료를 테스트 통과나 업무 성공으로 표시하지 않는다. 최종 상태도 `응답 완료`다.
- 실패·중단·입력 필요는 실제 턴 상태를 따른다. 도구 오류 후 진행이 이어지면 과거 오류 때문에 전체 작업을 실패로 표시하지 않는다.
- 진행 설명은 공개 메시지의 일부이며, 추론 원문은 현황에 복사하지 않는다.
- 다음 작업은 명시적인 계획 기록이 있을 때만 표시한다. 계획 기록이 없는 CLI에서는 만들어내지 않는다.
- 새 폴링, 타이머, 애니메이션 없이 기존 피드 이벤트를 사용한다. 변하지 않은 현황은 다시 발행하지 않는다.

## 검증

실행 명령:

```sh
python3 scripts/office-test.py --kind swift -- swift test --filter 'ConversationWorkProgressTests|LiveFeedStoreTests|ConversationWorkspaceTests|TerminalWorkspaceLifecycleTests|OfficeGamePaletteTests|OfficeLocalizationTests'
```

113개 중 111개 통과, 실패 0, 선택 실행형 기존 성능 진단 2개 건너뜀.
새 9개 검사는 도구 판별, 결과 과장 방지, 과거 실행 상태 우선순위, 오류 후 복구,
명시적 계획·공개 메시지, 기록 길이 제한, 숨겨진 피드 갱신, 직원 분리, 테마 렌더링을 확인한다.
320pt·720pt 라이트·다크의 네이티브 컴포넌트 PNG 4개를 렌더링해 확인했다.
실행 앱 전체를 조작한 검증과는 구분한다. 기존 SwiftTerm 증분 빌드 경고가 있었고 테스트는 정상 종료했다.

`zsh scripts/build-app.sh` 성공. `dist/OFFICESTRA.app` 교체, 번들 임베딩 smoke와 서명 검증 통과.
빌드 로그는 `/tmp/officestra-work-progress-build.log`이며 기존 npm `boolean@3.2.0` deprecation 경고가 있다.
실행 중인 앱과 4317 백엔드는 재시작하지 않았다. 새 UI는 앱 재시작 후 적용된다.
현재 백엔드 `/health`는 `ok=true`, `acceptingJobs=true`, `draining=false`로 확인했다.

## 변경 파일

- `Sources/OfficeGame/ConversationWorkProgress.swift`
- `Sources/OfficeGame/ConversationWorkProgressView.swift`
- `Sources/OfficeGame/AgentDirector.swift` — 별도 현황 저장소 연결 3줄
- `Sources/OfficeGame/ConversationWorkspace.swift` — 공통 현황 표시 연결
- `Sources/OfficeGame/Resources/en.lproj/Localizable.strings` — 새 UI 번역만 추가
- `Tests/OfficeCoreTests/ConversationWorkProgressTests.swift`
- `docs/ui-design-system.md` — 현황 표시 기준 추가
- 이 보고서

기존 공동 작업의 수정은 보존했다. 백엔드 소스·DB·기존 전사 표시 로직은 이번 작업에서 수정하지 않았다.
커밋·푸시는 하지 않았다.

## 앱 재시작 후 실제 화면 점검

사용자의 테스트 요청으로 22:58:39에 시작된 새 앱(PID 9560)을 확인했다.
현재 GUI에서 현황 줄과 상세 팝오버가 표시되고, 테스트 실행이 `수정한 내용 검증 중`으로
기록되는 것을 접근성 트리와 실제 화면으로 확인했다. 분할 상태에서 다른 직원을 선택하면
이름·응답 완료 상태·진행 설명이 함께 바뀌며, 원래 직원으로 돌아오면 현재 작업 표시로 복구됐다.

`ConversationWorkProgressTests` 재실행: 9개 통과, 실패·건너뜀 0.
로그: `/var/folders/hq/p1hq4jyj4x9dh0f6xlwsh7x80000gn/T/office-test-rjadultw.log`.
직원이 작업 중인 동안 터미널 전환이 비활성화되는 기존 정책을 유지했다. 이번 실제 화면
점검에서 터미널 전환을 강제로 실행하지 않았으며, 터미널 데이터 연결은 앞선 자동 검사 범위다.
UI 조작 중 사용자 화면 변경으로 한 차례 자동화가 중단됐고 상태를 다시 조회해 이어서 검증했다.
제품 소스 수정은 없고 이 검증 기록만 추가했다.

## 터미널 실제 테스트에서 확인한 누락과 보완

사용자가 터미널로 테스트한 턴에서 실제 UI는 터미널 모드였지만 현황 줄은
`다음 작업 진행 중 / 새 작업 기록이 도착하면 여기에 표시합니다`에 머물렀다.
라이브 피드에도 Graft 활동만 있었고, CLI의 공개 진행 설명과 명령 기록은 없었다.
앞선 컴포넌트 자동 검증만으로 터미널 실행 중 전달까지 확인한 것은 아니었다.

원인은 터미널 활동을 대부분 완료 훅에서만 가져오던 백엔드 경로였다.
Codex의 기존 rollout 감시, Antigravity의 기존 DB 감시에 실시간 활동 발행을 연결했다.
Claude는 UserPromptSubmit의 파일 위치부터 실행 중인 파일의 변경 이벤트만 감시한다.
대화 파일이 시작 훅 이후 생성돼도 읽으며, 완료·중단·닫기 시 감시를 해제한다.
유휴 폴링이나 별도 CLI/모델 호출은 추가하지 않았다.

활동은 기존 개발 도구 이벤트와 같은 writer·순서·event key를 사용한다.
실행 중/완료/실패는 같은 기록을 갱신하고, 변하지 않은 스냅샷은 발행하지 않는다.
다른 턴·세션·sidechain 기록을 제외하고 명령의 자격 증명은 기존 파서로 가린다.
긴 최종 답변이 실시간 메시지에 먼저 실렸어도 완료 때 그 중복 활동만 제거하며,
최종 답변 본문과 앞선 진행 기록은 보존한다.

검증 명령:

```sh
python3 scripts/office-test.py --cwd backend --kind node -- node --test test/terminal-live-progress.test.mjs test/terminal-turn-activities.test.mjs test/codex-rollout-turns.test.mjs test/terminal-sessions.test.mjs test/developer-tool-events.test.mjs test/agent-runtime.test.mjs
```

206개 통과, 실패·건너뜀 0.
로그: `/var/folders/hq/p1hq4jyj4x9dh0f6xlwsh7x80000gn/T/office-test-feviafts.log`.
새 검사는 실제 파일 변경 이벤트, 새 파일 생성, UTF-8 분할 입력, 턴 경계,
완료 전 공개 메시지와 도구 상태 전달, 기존 writer 공유, 중복 없는 갱신,
긴 최종 답변 중복 정리를 확인한다. 테스트의 잠긴 DB·최종 기록 지연·중복 notify
경고와 Node SQLite 실험 경고가 있었으며 실패는 없었다.

이번 보완에서 수정한 파일:

- `backend/src/terminal-live-progress.mjs` (신규)
- `backend/src/terminal-turn-activities.mjs`
- `backend/src/terminal-sessions.mjs`
- `backend/src/agent-runtime.mjs`
- `backend/test/terminal-live-progress.test.mjs` (신규)
- `backend/test/terminal-sessions.test.mjs`
- `backend/test/agent-runtime.test.mjs`
- 이 보고서

공동 작업의 기존 변경은 유지했다. 실행 중인 4317 백엔드는 정상이고 현재 테스트 턴이
진행 중임을 확인했다. 사용자 지침에 따라 백엔드는 재시작하지 않았다.
재시작 후 실제 터미널 화면에서 새 기록이 올라오는 검증은 아직 남아 있다.

`zsh scripts/build-app.sh` 성공(컴파일 71.43초). `dist/OFFICESTRA.app`를 교체했고
번들 임베딩 smoke·서명 검증을 통과했다. 기존 `boolean@3.2.0` deprecation 경고가 있었다.
빌드 로그: `/tmp/officestra-terminal-progress-build.log`.
근거 별도 제출은 현재 터미널 도구 환경에 `OFFICESTRA_RESULT_PATH`가 없어 실행하지 못했다.
