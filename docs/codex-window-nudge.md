# Codex 5시간 구간 시작 작업

기존 Claude `claude-window-nudge.py`는 유지한다. Codex 전용 작업은
`scripts/codex-window-nudge.py`이며 `com.neo.office-codex-window-nudge` LaunchAgent가
120초마다 확인한다. 모델 요청은 서버가 알려 준 5시간 초기화 시각 + 1분에만 한다.

- GPT-6 Luna (`gpt-6-luna`), low, Standard. 요청은 `Reply exactly OK. Do not use tools.`
- 로그인한 ChatGPT 구독을 사용한다. 별도 API 키 환경은 넘기지 않는다.
- 임시 폴더의 새 `--ephemeral` 세션으로 실행한다. 사용자 설정/MCP/프로젝트
  대화 이력을 읽지 않고, 읽기 전용 샌드박스를 사용한다. 직원에게 메시지를 보내지 않는다.
- 이 요청에서만 기본 모델 안내를 한 줄로 대체하고 `AGENTS.md` 주입 한도를 0으로
  둔다. 스킬 탐색·메모리·플러그인·앱·셸·브라우저 등도 이 요청에서만 비활성화한다.
  일반 직원 세션의 설정과 안내는 바꾸지 않는다.
- 새 5시간 구간이 이미 관측되면 그 초기화 시각으로 일정을 옮겨 불필요한 요청을 생략한다.
- 요청 전 상태 파일을 원자적으로 저장하고 실행 파일 잠금을 잡는다. 재시작·타임아웃에도
  같은 구간의 요청을 다시 보내지 않는다. 상위 모델 재시도도 없다.
- 요청 후 공급자 초기화 시각이 실제로 진전하는지 확인한다. 45분 동안 확인하지
  못하면 자동 요청을 중지한다. 초기화 시각/잔여량을 못 읽거나 오래된 값이면 대기한다.
- 주간 잔여량이 0이면 주간 초기화까지 대기한다. 맥이 꺼지거나 잠자면 그동안 실행되지 않는다.
- 응답 토큰 수만 CLI 보고값으로 로그에 남긴다. 구독 소비나 비용을 임의 계산하지 않는다.

상태: `~/Library/Application Support/OFFICESTRA/codex-window-nudge.json`

로그: `~/Library/Logs/office-codex-window-nudge.log`, `office-codex-window-nudge.error.log`

설치: `scripts/com.neo.office-codex-window-nudge.plist`를 `~/Library/LaunchAgents/`에
복사하고 `launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.neo.office-codex-window-nudge.plist`.
이 작업 등록은 4317 백엔드를 재시작하지 않는다.

확인: `launchctl print gui/$(id -u)/com.neo.office-codex-window-nudge`

중지: `launchctl bootout gui/$(id -u)/com.neo.office-codex-window-nudge`

테스트: `python3 scripts/test-codex-window-nudge.py`

참고: 공식 모델/사용량 안내 https://learn.chatgpt.com/docs/pricing
사용량 한도는 실제 계정 API 값을 따른다. 짧은 요청 자체도 구독 사용량을 소비하며,
제공자가 실제 다음 구간을 시작했다는 판정은 초기화 시각의 변경을 확인한 뒤에만 한다.
