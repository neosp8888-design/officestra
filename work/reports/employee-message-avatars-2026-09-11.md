# 직원 메시지 발신자 아이콘

대화 말풍선, 보관함 타일·상세, 기존 대화 이력 화면에 발신 직원의 얼굴과 이름을 표시한다. 기존 아바타 캐시를 사용하며 추가 네트워크 요청·호버 감지는 없다.

## 저장과 호환

- POST /api/agent-jobs의 선택 필드 senderCharacterId를 turns.sender_character_id에 저장한다. 존재하지 않는 발신 직원은 400으로 거절한다.
- GUI 및 터미널 dispatch의 실제 턴 접수까지 발신 정보를 유지한다. 일반 사용자 입력과 일치하지 않는 terminal dispatch에는 발신자를 붙이지 않는다.
- 신규 CLI 실행 지침에 발신 필드를 안내한다. 발신 정보는 표시용이며 인증·권한 근거가 아니다.
- 발신 정보가 없는 과거 메시지는 첫머리의 명시적 자기소개(선택적인 대괄호 머리말 뒤 포함)만 제한적으로 인식한다. 단순한 직원 이름 언급에는 표시하지 않는다. 옛 이름으로 쓰인 자기소개와 임의 인용을 완벽하게 구분하는 인증 기능은 아니다.
- 기존 기록은 수정하지 않았다. 마이그레이션 040은 nullable FK 컬럼만 추가했다.

## 검증

- 백엔드 전체: 599개 중 592 통과, 7 건너뜀, 실패 0.
- Swift 전체: 593개 중 592 통과, 새 오류문구 영문 번역 누락 1건 실패.
- 해당 번역 추가 후 EmployeeMessageSenderTests + OfficeLocalizationTests 18개 전부 통과. 수정 후 전체 Swift 재실행은 하지 않았다.
- 신규 발신자 테스트: 명시 ID/이름 변경, 과거 자기소개, 사용자 언급·인용 제외, 이전 JSON 호환.
- 마이그레이션: 1개 적용, 39개 건너뜀. 실제 읽기 전용 SQL에서 새 컬럼과 발신자 조회식 확인.
- scripts/build-app.sh 완료 및 번들 서명 검증 통과.
- 백엔드 4317 PID 3646 유지. 앱·백엔드 재시작, 커밋·푸시 없음. 실제 앱 화면의 아이콘 및 신규 API 경로는 재시작 후 확인 필요.

## 수정 파일

- Sources/OfficeGame/EmployeeMessageSender.swift (신규)
- Sources/OfficeGame/OfficeDatabaseClient.swift
- Sources/OfficeGame/OfficeDashboardPanels.swift
- Sources/OfficeGame/ConversationHistoryView.swift
- Sources/OfficeGame/ArchiveBookshelfView.swift
- Sources/OfficeGame/Resources/SystemMessages.en.json
- Tests/OfficeCoreTests/EmployeeMessageSenderTests.swift (신규)
- backend/src/server.mjs
- backend/src/agent-runtime.mjs
- backend/src/terminal-sessions.mjs
- backend/src/structured-turn-result.mjs
- backend/test/agent-runtime.test.mjs
- backend/test/terminal-sessions.test.mjs
- backend/test/structured-turn-result.test.mjs
- database/migrations/040_turn_sender.sql (신규)
- work/reports/employee-message-avatars-2026-09-11.md (이 보고서)
