# 업무 보드

프로젝트와 티켓은 PostgreSQL에 저장됩니다. 앱 왼쪽 아래 정보 패널의 **업무 보드** 탭에는 프로젝트 요약만 표시됩니다. 프로젝트를 누르면 넓은 별도 창이 열리고, 그 창에서 **대시보드·WBS·칸반**을 전환하며 같은 티켓을 볼 수 있습니다. 대시보드는 미완료 작업과 최근 완료를, WBS는 부모·자식 관계를, 칸반은 상태별 열을 보여줍니다. 같은 데이터는 모든 CLI·모델·GUI·터미널에서 아래 로컬 API로 읽고 쓸 수 있습니다.

| 메서드 | 경로 | 용도 |
| --- | --- | --- |
| GET | `/api/work-board` | 프로젝트·티켓·목표 전체 조회 |
| POST | `/api/work-board/projects` | 프로젝트 생성 |
| PATCH | `/api/work-board/projects/:id` | 프로젝트 이름·설명 수정 |
| POST | `/api/work-board/tickets` | 티켓 생성 |
| PATCH | `/api/work-board/tickets/:id` | 티켓 수정 |
| POST | `/api/work-board/goals` | 주간·월간 목표 생성 |
| PATCH | `/api/work-board/goals/:id` | 목표 수정 |
| GET | `/api/work-board/records/:id` | 연결 기록 본문을 열 때 조회 |
| GET | `/api/work-board/tickets/:id/activity` | 티켓의 최근 진행·변경 기록 100건 조회 |
| POST | `/api/work-board/tickets/:id/activity` | 진행·결정·검증 메모 추가 |

프로젝트 생성에는 `projectKey`(소문자·숫자·하이픈 고유 키)와 `title`이 필요합니다. 티켓 생성에는 `projectId`(UUID)와 `title`이 필요합니다. 선택 필드는 `description`, `assigneeId`(직원 ID 또는 null), `completedById`(완료자 ID 또는 null), `verifiedById`(검증자 ID 또는 null), `state`, `dueDate`(`YYYY-MM-DD` 또는 null), `completionCriteria`입니다. 상태는 `todo`, `in_progress`, `blocked`, `done` 중 하나이며 기본값은 `todo`입니다. PATCH는 보낸 필드만 바꿉니다. 담당자·완료자·검증자는 서로 다른 의미이고, 근거가 없는 과거 티켓의 완료자·검증자는 미확인으로 남깁니다. 이 값은 업무 근거를 보고 수동으로 지정하며 로그인 신원 인증을 뜻하지 않습니다. 변경 요청은 `Content-Type: application/json`을 사용합니다.

## 2단계 관계와 목표

티켓에는 `parentTicketId`(UUID 또는 null), `predecessorIds`(UUID 배열), `goalIds`(UUID 배열)가 추가됩니다. 셋 모두 같은 프로젝트의 대상만 허용합니다. 부모와 선행 관계의 자기 참조·순환은 거부하며, 배열은 PATCH에서 전체 교체합니다. 빈 배열을 보내면 해당 연결을 모두 해제합니다. 앱의 **대시보드/WBS/칸반** 전환은 동일한 티켓 데이터를 다른 방식으로 보여줍니다.

목표 생성에는 `projectId`, `horizon`(`weekly` 또는 `monthly`), `periodStart`(`YYYY-MM-DD`), `title`이 필요합니다. 주간 시작일은 월요일, 월간 시작일은 매월 1일을 명시해서 입력합니다. 선택 필드는 `description`, `metricName`, `metricUnit`, `targetValue`, `actualValue`입니다. 수치 필드는 최대 소수 넷째 자리의 **문자열** 또는 null이며, 목표값은 0보다 커야 합니다. 수치가 있으면 지표 이름이 필요합니다. 앱은 실제값과 목표값을 따로 보여주며, 티켓 완료 수를 KPI 달성률로 취급하지 않습니다. 목표나 수치는 초기값으로 채우지 않습니다.

초기 프로젝트는 Toss 주식·트레이딩, 오피스 최적화, TTS 설정 세 개입니다. 고유 키 충돌을 막아 재실행해도 중복 생성되지 않습니다. 과거 대화 기록에서 상태·기한·완료율을 추측해 채우지 않습니다. 기존 `projects`는 저장소별 업무 기록용이며 이 보드의 프로젝트와 다릅니다. 담당자 지정은 티켓 데이터만 바꾸며 직원에게 실행 요청을 보내지 않습니다.

## 3단계 업무 기록 연결

티켓 생성·수정에 `workRecordIds`(기존 업무 기록 UUID 배열)를 보낼 수 있습니다. PATCH에서 보내면 전체 교체하고, `[]`이면 연결을 해제합니다. 없는 기록 ID와 중복 ID는 거부합니다. `GET /api/work-board`는 연결 기록의 제목·작성자·기록 종류·시각·원래 대화 ID만 간략히 돌려줍니다. 본문은 `GET /api/work-board/records/:id`로 열 때 조회합니다. 앱 티켓 편집 화면에서는 검색 버튼으로 기존 `GET /api/work-records`의 기록을 찾아 연결하고 내용을 볼 수 있습니다. 검색은 버튼을 눌렀을 때만 실행하며 자동 폴링하지 않습니다.

연결은 기존 기록을 가리키는 참조입니다. `lifecycleState=active`는 기록 보존 상태이고 현재 직원 실행 여부가 아닙니다. 티켓 배정·기록 연결만으로 직원 업무가 실행되지는 않습니다. 실제 실행 요청과 결과 자동 연결은 별도 단계입니다. 현재 보드의 상태별 티켓 개수는 KPI 달성률이 아닙니다.

## 진행 기록과 사용자 결정 대기

티켓의 `decisionPending`은 불리언이며 기본값은 `false`입니다. `true`는 상태와 별도로 **사용자 결정 대기** 배지를 표시합니다. 기존 티켓의 설명을 보고 자동으로 추정하지 않습니다.

진행·결정·검증 내용은 `POST /api/work-board/tickets/:id/activity`에 `{ "kind": "progress", "body": "측정 결과…" }`처럼 추가합니다. `kind`는 `progress`, `decision`, `verification` 중 하나입니다. 메모는 덮어쓰거나 삭제하는 API가 없고, 티켓 수정 시 변경된 필드의 이전값과 새값도 별도 이력으로 남습니다. 목록 API는 최근 100건만 요청할 때 읽으므로 보드 전체 조회에 기록 본문을 실어 보내지 않습니다.

`reportedActorId`를 메모나 티켓 생성·수정 요청에 선택적으로 넣을 수 있습니다. 이는 호출자가 **신고한 작성자**일 뿐 인증된 신원이 아닙니다. 생략하면 작성자 미확인으로 표시합니다. 정확한 사용자별 감사 추적에는 호출자 인증이 추가로 필요합니다. 현재 턴의 업무 기록을 자동 연결하는 기능은 아직 없으며, 기존 기록은 턴 종료 후 `workRecordIds`로 연결합니다.
