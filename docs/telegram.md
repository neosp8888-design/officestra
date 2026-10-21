# 텔레그램 직원 대화

기존 4317 백엔드를 사용하는 독립 연결 프로세스다. 백엔드나 앱의 재시작 없이
`scripts/office-telegram.mjs`를 사용자 LaunchAgent로 실행한다. 추가 포트나 웹훅은 열지 않는다.
직원 목록·현재 접수 상태·실행·답변은 기존 API를 사용한다.

- `/office`: 실시간 직원 목록에서 선택. 이후 일반 텍스트를 선택한 직원에게 전달한다.
- `클대리: 질문` 또는 `@클대리 질문`: 해당 직원에게 직접 전달한다. 실제 앱 직원 이름을 사용한다.
- 직원 답장에 답글을 달면 그 직원에게 이어진다. 다른 직원을 선택해도 이전 답글의 대상은 유지된다.
- `/status`: 현재 직원 접수 가능 여부. 작업 중이면 실행을 중단하거나 중복 요청하지 않는다.
- `/new`: 선택한 직원의 다음 메시지를 새 대화로 시작한다. 기존 기록은 삭제하지 않는다.
- 직원별 대화 ID를 보존해 앱과 텔레그램에서 같은 실행 결과를 볼 수 있다.
- 앱에서 롤플레이가 켜진 직원은 매 메시지마다 현재 모드를 확인해 기존 롤플레이
  메시지 API로 전달한다. 일반 업무 CLI를 실행하거나 모델·프리셋·토글·진행 중인 대화를 바꾸지 않는다.
  선택 메뉴에 `롤플레이`가 표시되고, 현재 앱 대화에서 생성된 새 답변만 가져온다.
- 롤플레이 중 `/new`를 직접 보내면 앱의 새 롤플레이 대화 API를 사용한다.
  앱에서 대화를 바꿨거나 여러 입력이 겹쳐 답변 대상을 확정할 수 없으면 다른 대화의 답을 전송하지 않는다.
- 현재는 텍스트 대화만 지원한다. 긴 답변은 텔레그램 길이 제한에 맞춰 나누어 보낸다.

설정: `~/.config/officestra/telegram.env`의 `TELEGRAM_BOT_TOKEN` (권한 0600).
상태: 같은 폴더의 `telegram-state.json` (권한 0600). 허용된 개인 대화와 발신자 ID,
직원별 대화 ID, 처리한 update offset, 아직 전달하지 않은 답변을 저장한다.
토큰과 대화 본문을 운영 로그에 기록하지 않는다. 상태 파일에는 미전달 답변이 잠시 포함될 수 있다.

설치 시 `node scripts/office-telegram.mjs --pair`로 최근 2시간 안에 `/start`를 보낸
단일 개인 사용자를 등록한다. 이는 이 대화에서 본인 송수신을 확인한 직후에만 수행하는
관리자 초기화 절차이며, 기존 상태를 덮어쓰지 않는다. 실행 중에는 신규 사용자를 자동 등록하지 않는다.
`--announce`는 직원 선택 메뉴와 봇 명령어를 등록하고 종료한다.

재시작 후 기존 응답 전달을 이어간다. 직원 실행 POST는 자동 재시도하지 않는다.
실행 접수/텔레그램 전송 도중 프로세스가 끊겨 결과가 불명확하면 중복 실행·중복 답장보다
앱 기록 확인 안내를 우선한다. 네트워크 경계에서 exactly-once 전달을 보장하지 않는다.
기존 웹훅이나 다른 poller가 있으면 교체하지 않고 오류로 중단한다.
Telegram의 재시도 대기 시간을 존중하며, 봇을 차단한 대화방에는 무한 재전송하지 않는다.

운영 등록 예시: `scripts/com.neo.officestra-telegram.plist`의 Node·저장소·로그 경로를
해당 설치 환경에 맞추고 `~/Library/LaunchAgents/`에 복사한 뒤
`launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.neo.officestra-telegram.plist`.
점검은 `launchctl print gui/$(id -u)/com.neo.officestra-telegram`, 연결 프로세스만 중지하려면
`launchctl bootout gui/$(id -u)/com.neo.officestra-telegram`을 사용한다.

검증: `python3 scripts/office-test.py --cwd backend --kind node -- node --test test/telegram-bridge.test.mjs`.
Telegram API 계약: https://core.telegram.org/bots/api#getupdates
