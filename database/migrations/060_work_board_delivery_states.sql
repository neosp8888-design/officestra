-- 오피스 티켓의 완료는 원격 반영 근거를 확인한 뒤에만 기록한다.
ALTER TABLE work_board_tickets
    ADD COLUMN IF NOT EXISTS pushed_commit_sha text;
ALTER TABLE work_board_tickets
    DROP CONSTRAINT IF EXISTS work_board_tickets_state_check;

UPDATE work_board_tickets AS ticket
SET state = 'review', updated_at = now()
FROM work_board_projects AS project
WHERE ticket.project_id = project.id
  AND project.project_key <> 'toss-trading'
  AND ticket.state = 'done'
  AND ticket.pushed_commit_sha IS NULL;

UPDATE work_board_tickets SET state = 'open' WHERE state = 'todo';
UPDATE work_board_tickets SET state = 'deferred' WHERE state = 'blocked';

ALTER TABLE work_board_tickets
    ALTER COLUMN state SET DEFAULT 'open',
    ADD CONSTRAINT work_board_tickets_state_check
        CHECK (state IN ('open', 'in_progress', 'review', 'done', 'canceled', 'deferred')),
    ADD CONSTRAINT work_board_tickets_pushed_commit_sha_check
        CHECK (pushed_commit_sha IS NULL OR pushed_commit_sha ~ '^[0-9a-f]{40}$');
