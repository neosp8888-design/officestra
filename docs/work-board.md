# 업무 보드

프로젝트와 티켓은 PostgreSQL에 저장됩니다. 앱 왼쪽 사무실 그림 바로 아래 정보 패널의 제목 자리에는 대화 보관함·화이트보드·사내 위키·업무 보드 선택 버튼 네 개가 표시됩니다. **업무 보드**를 고르면 같은 패널에 프로젝트 요약이 표시됩니다. 프로젝트를 누르면 넓은 별도 창이 열리고, 그 창에서 **대시보드·WBS·칸반**을 전환하며 같은 티켓을 볼 수 있습니다. 대시보드는 미완료 작업과 최근 완료를, WBS는 부모·자식 관계를, 칸반은 상태별 열을 보여줍니다. 같은 데이터는 모든 CLI·모델·GUI·터미널에서 아래 로컬 API로 읽고 쓸 수 있습니다.

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

프로젝트 생성에는 `projectKey`(소문자·숫자·하이픈 고유 키)와 `title`이 필요합니다. 티켓 생성에는 `projectId`(UUID)와 `title`이 필요합니다. 선택 필드는 `description`, `assigneeId`(직원 ID 또는 null), `completedById`(완료자 ID 또는 null), `verifiedById`(검증자 ID 또는 null), `state`, `ticketType`, `dueDate`(`YYYY-MM-DD` 또는 null), `completionCriteria`, `completionEvidence`, `pushedCommitSha`(40자리 소문자 SHA 또는 null)입니다. 상태는 `open`(오픈), `in_progress`(진행), `review`(검토), `done`(완료·반영), `canceled`(취소·반영 안 함), `deferred`(대기·추후 반영) 중 하나이며 기본값은 `open`입니다. PATCH는 보낸 필드만 바꿉니다. 담당자·완료자·검증자는 직원 ID로 서로 다른 의미를 가지며, 사용자 검토는 별도 필드로 기록합니다. 근거 없는 과거 티켓의 완료자·검증자·유형은 미확인으로 남깁니다. 이 값은 업무 근거를 보고 수동으로 지정하며 로그인 신원 인증을 뜻하지 않습니다. 변경 요청은 `Content-Type: application/json`을 사용합니다.

## 유형·생명주기·완료 판정

`ticketType`은 **일의 산출물**, `state`는 **현재 단계**입니다. `general`은 기존 티켓과 구형 API를 위한 미분류 기본값이며 063 마이그레이션은 과거 완료를 재판정하거나 유형을 추측하지 않습니다. 앱에서 새 티켓을 만들 때는 유형을 선택해야 합니다. 유형을 지정한 새 티켓은 `open`으로 만들고 `open → in_progress → review → done`을 따릅니다. 검토에서 수정하면 `in_progress`로 되돌립니다. 진행 중에는 `deferred`·`canceled`로 옮길 수 있고 `resolutionReason`이 필요합니다. 대기에서 재개할 때는 `open`으로 돌아갑니다. 완료·취소는 종료 상태입니다. 검토에 올린 티켓은 미분류를 포함해 진행으로 먼저 되돌린 뒤 별도 요청에서 유형을 지정해야 합니다. 한 번의 요청으로 유형과 완료 상태를 함께 바꿔 SHA 조건을 피할 수 없습니다. 기존 미분류 티켓의 전이 관용성은 유지하지만, 직원이 선택한 유형의 실제 적합성을 인증하거나 자동 판정하지는 않습니다.

| 유형 | 코드 | 검토에 올릴 결과 근거 | 완료에 추가로 필요한 조건 |
| --- | --- | --- | --- |
| 일반·미분류 | `general` | 기존 규칙 유지 | Toss 외 프로젝트는 원격 푸시 SHA |
| 기획 | `planning` | 범위·대안·결정문 | 사용자 검토 승인 기록 |
| 구현 | `implementation` | 변경 내용·로컬 테스트 결과 | Toss 외 프로젝트는 원격 푸시 SHA |
| 사전테스트 | `pretest` | 재현 절차·실행 결과·남은 실패 | 사용자 검토 선택 |
| 독립검증 | `verification` | 대상·재현 결과·합격/불합격 판정 | 대상 담당자·완료자와 다른 검증 완료자 |
| 조사 | `research` | 출처·확인 시각·결론·불확실성 | 사용자 검토 선택 |
| 콘텐츠 제작 | `content` | 결과물과 검토 내용 | 사용자 검토 승인 기록 |
| 운영 | `operations` | 실행 결과와 영향 범위(`operationImpact`) | 외부 영향이면 사용자 검토 승인 기록 |

유형이 있는 티켓은 `review`에 들어갈 때 `completionCriteria`, `completionEvidence`, `completedById`가 필요합니다. `done`은 반드시 `review`를 거치고 `decisionPending=false`여야 합니다. `verification`은 `targetTicketId`(같은 프로젝트)와 완료 시 `verificationVerdict`(`pass`/`fail`)가 필요합니다. 대상은 검토 또는 완료 상태여야 합니다. 합격이면 대상의 `verifiedById`를 검증 완료자로 기록하고, 불합격이면 같은 트랜잭션에서 대상을 `in_progress`로 돌려 이전 완료자·완료 근거·푸시 SHA·사용자 검토 기록을 비웁니다. 검증 티켓을 다시 독립검증하는 중복 관문은 두지 않습니다.

`userReview`는 `none`, `approved`, `changes_requested` 중 하나이며 `approved` 또는 `changes_requested`에는 `userReviewNote`가 필요합니다. 생성과 수정 모두 검토 결과를 기록할 때 `reportedActorId`로 `user` 또는 등록된 직원 ID를 신고해야 합니다. 서버가 `userReviewedAt`을 기록하고 티켓 이력에도 신고된 기록자를 남깁니다. 앱의 **사용자 검토(기록)**에는 사용자·직원 선택지가 있지만, 사용자의 직접 승인과 동일한 인증 증명이 아닙니다. API에 호출자 인증이 없으므로 기록 주체는 신고값과 메모로만 추적합니다. 직원 검증자 `verifiedById`와 혼용하지 않습니다. 승인 후 검토에서 진행으로 되돌리거나 결과물을 바꾸면 승인 기록은 초기화됩니다.

Toss 외 프로젝트의 `implementation`·`general`만 `done`에 원격에서 확인한 `pushedCommitSha`가 필요합니다. 다른 유형은 SHA 없이 유형별 결과와 검토 조건으로 완료할 수 있습니다. API는 SHA 형식과 필수 여부만 검사하며 실제 원격 포함 여부는 작업자가 별도로 검증해야 합니다. 060 마이그레이션은 당시 푸시 근거가 없는 기존 `done`을 `review`로 옮겼고, 063은 그 판정을 다시 바꾸지 않습니다. Toss는 별도 저장소이므로 이 SHA 규칙을 적용하지 않습니다.

## 2단계 관계와 목표

티켓에는 `parentTicketId`(UUID 또는 null), `predecessorIds`(UUID 배열), `goalIds`(UUID 배열)가 추가됩니다. 셋 모두 같은 프로젝트의 대상만 허용합니다. 부모와 선행 관계의 자기 참조·순환은 거부하며, 배열은 PATCH에서 전체 교체합니다. 빈 배열을 보내면 해당 연결을 모두 해제합니다. 앱의 **대시보드/WBS/칸반** 전환은 동일한 티켓 데이터를 다른 방식으로 보여줍니다.

목표 생성에는 `projectId`, `horizon`(`weekly` 또는 `monthly`), `periodStart`(`YYYY-MM-DD`), `title`이 필요합니다. 주간 시작일은 월요일, 월간 시작일은 매월 1일을 명시해서 입력합니다. 선택 필드는 `description`, `metricName`, `metricUnit`, `targetValue`, `actualValue`입니다. 수치 필드는 최대 소수 넷째 자리의 **문자열** 또는 null이며, 목표값은 0보다 커야 합니다. 수치가 있으면 지표 이름이 필요합니다. 앱은 실제값과 목표값을 따로 보여주며, 티켓 완료 수를 KPI 달성률로 취급하지 않습니다. 목표나 수치는 초기값으로 채우지 않습니다.

초기 프로젝트는 Toss 주식·트레이딩, 오피스 유지보수, TTS 설정 세 개입니다. 오피스 프로젝트의 고유 키 `office-optimization`은 기존 티켓 연결을 위해 유지하고, 옛 기본 이름만 마이그레이션으로 바꿉니다. 사용자가 따로 바꾼 이름은 보존합니다. 고유 키 충돌을 막아 재실행해도 중복 생성되지 않습니다. 과거 대화 기록에서 상태·기한·완료율을 추측해 채우지 않습니다. 기존 `projects`는 저장소별 업무 기록용이며 이 보드의 프로젝트와 다릅니다. 담당자 지정은 티켓 데이터만 바꾸며 직원에게 실행 요청을 보내지 않습니다. 큰 업무 창의 상단에는 프로젝트 추가·새로고침·프로젝트 수정·티켓 추가 버튼을 한 줄로 배치합니다.

## 3단계 업무 기록 연결

티켓 생성·수정에 `workRecordIds`(기존 업무 기록 UUID 배열)를 보낼 수 있습니다. PATCH에서 보내면 전체 교체하고, `[]`이면 연결을 해제합니다. 없는 기록 ID와 중복 ID는 거부합니다. `GET /api/work-board`는 연결 기록의 제목·작성자·기록 종류·시각·원래 대화 ID만 간략히 돌려줍니다. 본문은 `GET /api/work-board/records/:id`로 열 때 조회합니다. 앱 티켓 편집 화면에서는 검색 버튼으로 기존 `GET /api/work-records`의 기록을 찾아 연결하고 내용을 볼 수 있습니다. 검색은 버튼을 눌렀을 때만 실행하며 자동 폴링하지 않습니다.

연결은 기존 기록을 가리키는 참조입니다. `lifecycleState=active`는 기록 보존 상태이고 현재 직원 실행 여부가 아닙니다. 티켓 배정·기록 연결만으로 직원 업무가 실행되지는 않습니다. 실제 실행 요청과 결과 자동 연결은 별도 단계입니다. 현재 보드의 상태별 티켓 개수는 KPI 달성률이 아닙니다.

## 진행 기록과 사용자 결정 대기

티켓의 `decisionPending`은 불리언이며 기본값은 `false`입니다. `true`는 상태와 별도로 **사용자 결정 대기** 배지를 표시합니다. 기존 티켓의 설명을 보고 자동으로 추정하지 않습니다.

진행·결정·검증 내용은 `POST /api/work-board/tickets/:id/activity`에 `{ "kind": "progress", "body": "측정 결과…" }`처럼 추가합니다. `kind`는 `progress`, `decision`, `verification` 중 하나입니다. 메모는 덮어쓰거나 삭제하는 API가 없고, 티켓 수정 시 변경된 필드의 이전값과 새값도 별도 이력으로 남습니다. 목록 API는 최근 100건만 요청할 때 읽으므로 보드 전체 조회에 기록 본문을 실어 보내지 않습니다.

`reportedActorId`를 메모나 티켓 생성·수정 요청에 선택적으로 넣을 수 있습니다. 이는 호출자가 **신고한 작성자**일 뿐 인증된 신원이 아닙니다. 생략하면 작성자 미확인으로 표시합니다. 정확한 사용자별 감사 추적에는 호출자 인증이 추가로 필요합니다. 현재 턴의 업무 기록을 자동 연결하는 기능은 아직 없으며, 기존 기록은 턴 종료 후 `workRecordIds`로 연결합니다.
