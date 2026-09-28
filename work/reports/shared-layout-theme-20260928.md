# 공통 레이아웃 테마 적용

라이트/다크 공통 게임 카드 디자인을 대화창부터 전체 작업 화면의 도구로 확장했다.
GUI/터미널은 헤더, 입력, 설정, 분할 프레임을 공유한다. CLI 출력과 구문 강조 코드 본문은
기존 어두운 캔버스를 유지한다. 네이티브 메뉴·폼·경고창 및 특수 아바타 애니메이션은 유지한다.

## 변경 파일

- Sources/OfficeGame/OfficeGameDesign.swift — 반복 도구용 그림자 없는 공통 스타일
- Sources/OfficeGame/OfficeGameApp.swift — 창 기본 버튼, 테마/표현 방식/모드/서버, 하단 설정, 로컬 모델 도구
- Sources/OfficeGame/CommandComposerView.swift — 입력, 첨부, 전송, 중단, 예약
- Sources/OfficeGame/OfficeDashboardPanels.swift — 질문/답변 카드 및 대화 도구
- Sources/OfficeGame/ConversationWorkspace.swift — GUI/터미널 공통 프레임 및 분할 직원 선택
- Sources/OfficeGame/ConversationHistoryView.swift — 보관함 대화 카드
- Sources/OfficeGame/AgentActivityLogView.swift — 실행 기록 카드와 펼침 도구
- Sources/OfficeGame/InlineQuestionAnswerView.swift — 확인 요청 카드 및 도구
- Sources/OfficeGame/ResponseMessageFooter.swift — 평가/복사
- Sources/OfficeGame/TaskPromptViews.swift — 첨부 도구
- Sources/OfficeGame/ConversationCodeBlockView.swift — 코드 헤더/복사
- Sources/OfficeGame/ResponseSourceView.swift — 근거 도구
- Sources/OfficeGame/CharacterTurnCostSummary.swift — 비용 도구
- Sources/OfficeGame/UsageReportSheet.swift — 사용량 도구
- Sources/OfficeGame/WikiKnowledgeView.swift — 위키 도구
- Sources/OfficeGame/WorkBoardView.swift — 업무 보드 도구
- Sources/OfficeGame/ReplyRoutingView.swift — 자동 전달 선택 도구
- docs/ui-design-system.md — 적용 범위와 모드별 기준
- work/reports/shared-layout-theme-20260928.md — 이번 변경 목록 및 검증 기록

## 로컬 검증

관련 Swift 테스트 71개: 통과 69, 건너뜀 2, 실패 0.
건너뜀은 별도 활성화가 필요한 입력/분할 성능 진단이다. CPU 개선을 측정한 결과는 아니다.
로그: /var/folders/hq/p1hq4jyj4x9dh0f6xlwsh7x80000gn/T/office-test-vpk22nml.log

릴리즈 빌드와 서명 검증 통과: /tmp/officestra-layout-theme-20260928.log.
재실행 후 라이트/다크의 대화·입력·사용량 화면, 분할 화면과 직원 선택 연동을 시각 확인했다.
업무 보드 프로젝트 목록도 확인했다. 최종 화면은 다크·분할·현재 작업 직원 선택 상태다.
터미널 전환은 진행 중 업무 보호 정책으로 막혀 실제 터미널 화면의 새 빌드 검증은 남아 있다.
터미널 캐시/생명주기 및 모드별 입력 제어 테스트는 위 통과 개수에 포함된다.

사용자 확인과 릴리즈는 별도다. 커밋/푸시는 하지 않았다.
