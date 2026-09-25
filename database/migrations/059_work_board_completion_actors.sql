-- 배정된 담당자와 실제 완료·검증 담당자를 분리한다. 과거 티켓은 근거 없이 채우지 않는다.
ALTER TABLE work_board_tickets
    ADD COLUMN IF NOT EXISTS completed_by_id text REFERENCES characters(id) ON DELETE SET NULL,
    ADD COLUMN IF NOT EXISTS verified_by_id text REFERENCES characters(id) ON DELETE SET NULL;
