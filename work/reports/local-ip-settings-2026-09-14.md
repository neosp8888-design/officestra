# 로컬 PC IP 설정 — 2026-09-14

## 동작
- 직원 설정에 로컬 PC IPv4 입력란과 독립적인 '확인 후 주소 저장' 버튼을 추가했다.
- 로컬 직원에게만 표시한다. 현재 연결 주소를 불러오며 한글/영문을 지원한다.
- PUT /api/characters/:id/local-address는 IPv4 형식과 기존 SSH 호스트 키로의
  접속을 확인한 후 config.localHostAddress에 직원별 주소만 저장한다.
- 실행/준비/압축/터미널 사용 중 변경은 차단한다. 확인 실패 시 DB 변경을 롤백한다.
- 모델 카탈로그의 공용 주소, 모델, 추론, 과거 턴 스냅샷과 세션 ID는 바꾸지 않는다.
- IP는 접속 경로로 취급한다. 동일한 hostKeyAlias를 유지하는 주소 변경은
  기존 세션을 이어갈 수 있다. 계정·키 파일·hostKeyAlias·모델 변경은 계속 차단한다.
  실제 SSH 연결에서는 StrictHostKeyChecking=yes를 유지한다.
- 사용 중이 아닌 기존 연결은 검증된 새 경로에서 소유 자원을 정리한 뒤 닫는다.
  다음 요청은 새 주소로 연결한다. 다른 활성 로컬 연결은 강제로 중단하지 않는다.
- 저장 버튼은 IPv4만 받는다. 포트(현재2222), SSH 계정·키를 바꾸는 UI는 추가하지 않았다.

## 검증
- 백엔드 전체: tests621/pass614/fail0/skipped7.
- 최종 관련 테스트: tests33/pass32/fail0/skipped1.
- Swift 주소/카탈로그 표시 테스트: 7통과/실패0.
- Swift 전체: 603통과/실패0 (146.407초).
- npm backend check, git diff --check 통과.
- scripts/build-app.sh 완료. 패키지 임베딩 smoke 및 앱 서명 검증 통과.
  수정한 백엔드4모듈은 배포 번들과 원본이 바이트 단위로 일치한다.
- 기존 4317은 PID90064/release f70e7682로 유지했다. 새 기능은 사용자 재시작 후 적용된다.
- 기존 PC의 새 공인 IP222.109.147.73은 저장 전 읽기 전용 SSH 확인에서
  기존 호스트 키 검증을 통과했다. 설정 저장 자체는 실행 중인 구버전 API에 하지 않았다.
- 새 UI의 수동 화면 검증 및 실제 저장은 앱·백엔드 재시작 후 필요하다.

## 수정 파일
- Sources/OfficeGame/AgentDirector.swift
- Sources/OfficeGame/OfficeDatabaseClient.swift
- Sources/OfficeGame/OfficeGameApp.swift
- Sources/OfficeGame/Resources/SystemMessages.en.json
- Sources/OfficeGame/Resources/en.lproj/Localizable.strings
- Tests/OfficeCoreTests/LocalProviderPresentationTests.swift
- backend/src/local-profile-selection.mjs
- backend/src/local-provider-host.mjs
- backend/src/local-provider-service.mjs
- backend/src/server.mjs
- backend/test/local-profile-selection.test.mjs
- backend/test/local-provider-service.test.mjs
- work/reports/local-ip-settings-2026-09-14.md

백엔드 재시작 및 커밋·푸시는 수행하지 않았다.
