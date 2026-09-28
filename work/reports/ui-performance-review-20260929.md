# UI 성능 분석 — 2026-09-29

## 결론

분할 화면에서 입력창 줄 수가 늘어날 때의 레이아웃 지연이 과거 저장된 동일 진단 결과보다 증가했다. 일반 글자 입력과 본문 재조판, 창 리사이즈가 모두 악화됐다는 증거는 없다. 제품 소스는 이번 분석에서 수정하지 않았다.

우선 개선 대상은 입력창 높이 변경 → 두 대화 viewport 크기 변경 → 스크롤 위치 복원을 위한 동기 레이아웃 경로다. 새 디자인의 장식 계층은 추가 비용 후보이지만, 어느 스타일이 증가분을 만들었는지는 아직 분리 측정하지 않았다. 현재 디자인을 유지한 채 중복 계산을 줄이는 접근을 권한다.

## 실시간 관측

- 실행 앱 PID 23360, dist/OFFICESTRA.app. 최초 ps 순간값 CPU 17.5%.
- 00:37:45~00:37:56의 6회 표본: 앱 CPU 2.5~6.3%, 백엔드 0~0.4%, WindowServer 27.4~32.8%.
- 백엔드 정상, activeTurnCount=1. 이번 분석 자체가 진행 중이므로 완전한 앱 유휴 시험은 아니다.
- sample 보고의 physical footprint 421.4MB, peak 499.1MB. 누수 여부를 판단할 장기 관측은 하지 않았다.
- 5초 sample에서 메인 스레드는 대기가 많고, 활동 스택에는 SwiftUI/AppKit 레이아웃·정렬·화면 갱신 경로가 나타났다. 심볼 표본을 CPU 원인별 정확한 비율로 환산하지 않았다.
- 관측된 UI: 다크·2D·GUI·분할, 음성 꺼짐. 화면 설정을 바꾸거나 앱/백엔드를 재시작하지 않았다.
- Chrome 및 Parsec도 실행 중이다. WindowServer 수치를 오피스 단독 비용으로 볼 수 없다.

## 기존 성능 진단 실제 실행

선택 실행용 진단 3개를 활성화해 모두 통과했다. 입력 진단은 한 번 더 실행해 재현성을 확인했다. 건너뛴 시험은 없다.

| 지표 | 과거 기록 | 현재 1차 | 현재 재측정 |
|---|---:|---:|---:|
| 분할 한글 입력 후 배치 p95 | 2.08ms | 2.49ms | 2.39ms |
| 분할 줄바꿈 후 배치 p95 | 16.31ms | 28.46ms | 34.94ms |
| 분할 줄바꿈 배치 최대 | 32.07ms | 37.92ms | 54.77ms |
| 분할 한글 글자 삽입 p95 | 1.30ms | 1.42ms | 1.45ms |
| 입력 중 Markdown layout 횟수 | 0 | 0 | 0 |

줄바꿈 배치 p95는 과거 대비 약 1.75~2.14배다. 이는 전체 앱의 FPS나 모든 입력의 지연이 같은 배수로 나빠졌다는 뜻은 아니다. 한글 진단은 문자열 삽입이며 실제 IME 조합 전체를 측정하지 않는다.

리사이즈 진단은 긴 Markdown 대화 10개, 표 240개를 사용한다.

| 구간 | 과거 CPU 시간 → 현재 | 과거 배치 p95 → 현재 | 과거 평균 호출 → 현재 |
|---|---|---|---|
| 폭 왕복 | 1,895 → 1,075ms | 76.76 → 38.59ms | 18.60 → 19.71ms |
| 폭 한 방향 | 1,760 → 1,471ms | 71.01 → 38.38ms | 15.69 → 23.99ms |
| 높이 | 478 → 381ms | 16.84 → 13.10ms | 7.28 → 3.29ms |

이 결과에서는 리사이즈의 전반적 회귀를 확인하지 못했다. 비동기 크기 합치기의 적용 횟수도 달라 단일 숫자만으로 해석하면 안 된다.

분할 상호작용 진단의 현재 프로세스 CPU: 유휴 0.15%, 종료 후 유휴 0.50%, 반복 포커스 전환 30.51%, 반복 타이핑 8.75%, 반복 스크롤 16.22%, 복합 동작 38.62%. 모든 구간에서 문서 재생성·본문 layout·높이 재요청은 0회였다. 이 fixture에는 전체 사무실·업무 보드·실시간 서버 스트리밍이 포함되지 않는다.

### 비교의 한계

- 과거 입력 기록은 9월 22일, 리사이즈 기록은 9월 23일이다. 같은 진단 코드/fixture의 보관 결과와 비교했으며, 옛 실행 파일을 현재 환경에서 동시에 A/B한 결과가 아니다.
- 현재 시험은 debug 빌드의 오프라인 네이티브 fixture다. 실행 앱의 release 빌드·실제 큰 창·GPU 합성·마우스 움직임 전체와 동일하지 않다.
- 두 진단 파일은 이번 디자인 변경 전후 수정되지 않았다. 운영체제/도구/동시 실행 앱 등의 차이는 통제하지 못했다.
- 테스트 통과는 계측을 정상 완료했다는 의미다. 지연 수치에 대한 성능 합격 판정이 아니다.

## 코드와 최근 변경 비교

비교 기준은 게임 카드 공통화 커밋 e8763f6의 직전 코드와 현재 작업 트리다.

1. 공통 장식: OfficeGameSurface는 배경·그라데이션·상단선·테두리·클리핑을, 비compact 버튼은 그림자와 변형 계층을 추가했다. 정적 상태에서는 매 프레임 SwiftUI 갱신을 유발하는 타이머가 없다. 대화 영역 호버 추적도 이미 꺼져 있다. 따라서 단순히 호버를 다시 제거하는 작업은 우선순위가 낮다.
2. 입력 높이: CommandEntryRow.composerHeight가 변하면 ConversationWorkspaceView 아래 양쪽 호스트의 높이가 바뀐다. CachedLiveWorkspaceFeedsNSView.applyViewportBounds/restoreViewportAfterResize는 호스트·스크롤뷰·문서의 layoutSubtreeIfNeeded를 동기 호출한다. 창 드래그용 합치기는 있지만 일반 입력 높이 변경은 즉시 처리한다. 현재 관측과 가장 잘 맞는 비용 후보다.
3. 포커스: 기존 제한된 문서 캐시와 선택 분리는 동작한다. 새 ComposerCharacterPicker도 AgentDirector를 넓게 관찰하므로 선택·실행 상태와 무관한 갱신을 줄일 여지는 있다. 전체 앱에서의 기여도는 아직 미측정이다.
4. 분할 하단 흐름선은 기존 Core Animation 구현이며 최근 변경은 두께 2→4pt다. 새 반복 SwiftUI 타이머가 생긴 것은 아니다. 현재 메인 병목으로 지목할 증거가 없다.
5. 스트리밍은 기존 250ms 배치 갱신을 유지한다. 스트리밍 때 추가된 장식 계층의 비용은 별도로 계측해야 한다.

## 추천 순서

1. **입력 높이 변경 전용 경량 경로**: 폭·문서 내용이 같으면 재사용 가능한 문서 높이/viewport 정보를 활용하고 중복된 동기 배치를 합친다. 줄바꿈, 바닥 고정, 과거 위치 유지, 분할 양쪽 표시를 함께 검증한다. 예전 작업에서 강제 레이아웃을 무조건 빼면 스크롤 회귀가 있었으므로 해당 보정 규칙을 보존한다.
2. **장식 계층 단순화**: 동일한 색·그라데이션·테두리를 적은 그리기 계층으로 표현한다. 큰 패널의 반복 그림자·클리핑부터 별도 A/B하며 화면 전체를 비트맵으로 굳히는 접근은 하지 않는다.
3. **관찰 범위 축소**: 직원 선택/상태 버튼이 필요한 값만 구독하도록 분리하고, 스트리밍 갱신 중 전체 헤더·입력 영역의 불필요한 재계산을 계측한다.

## 근거와 산출물

- Sources/OfficeGame/CommandComposerView.swift:160, 266, 285, 674, 705
- Sources/OfficeGame/OfficeDashboardPanels.swift:2037, 2150, 2200, 2223
- Sources/OfficeGame/OfficeGameDesign.swift:40, 106, 193
- Sources/OfficeGame/ComposerCharacterPicker.swift:5
- Sources/OfficeGame/LightweightAnimationViews.swift:7
- Sources/OfficeGame/AgentDirector.swift:892, 2847
- Tests/OfficeCoreTests/ConversationWorkspaceTests.swift:11, 113, 501
- Tests/OfficeCoreTests/CachedLiveWorkspaceFeedsLifecycleTests.swift:9
- 과거 원문: work/validation/composer-latency-20260922/native-perf.json, work/validation/window-resize-20260923/final-live.json
- 현재 원문: /tmp/officestra-perf-20260929-composer.json, /tmp/officestra-perf-20260929-composer-repeat.json, /tmp/officestra-perf-20260929-split.json, /tmp/officestra-perf-20260929-resize.json
- 프로세스 표본: /tmp/officestra-ui-perf-current.sample.txt
- 테스트 로그: /var/folders/hq/p1hq4jyj4x9dh0f6xlwsh7x80000gn/T/office-test-lz3jvfw2.log, /var/folders/hq/p1hq4jyj4x9dh0f6xlwsh7x80000gn/T/office-test-pr_glga9.log

제품 코드·설정 수정, 앱/백엔드 재시작, 재배포, 커밋/푸시는 하지 않았다. 이번에 추가한 저장소 파일은 이 분석 보고서 하나다.
