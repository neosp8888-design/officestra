-- 유형은 산출물의 성격이고 상태는 진행 단계다. 기존 티켓은 재분류하지 않는다.
ALTER TABLE work_board_tickets
    ADD COLUMN IF NOT EXISTS ticket_type text NOT NULL DEFAULT 'general',
    ADD COLUMN IF NOT EXISTS completion_evidence text NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS user_review text NOT NULL DEFAULT 'none',
    ADD COLUMN IF NOT EXISTS user_review_note text NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS user_reviewed_at timestamptz,
    ADD COLUMN IF NOT EXISTS target_ticket_id uuid REFERENCES work_board_tickets(id) ON DELETE RESTRICT,
    ADD COLUMN IF NOT EXISTS verification_verdict text,
    ADD COLUMN IF NOT EXISTS operation_impact text,
    ADD COLUMN IF NOT EXISTS resolution_reason text NOT NULL DEFAULT '';

ALTER TABLE work_board_tickets
    ADD CONSTRAINT work_board_tickets_type_check
        CHECK (ticket_type IN ('general', 'planning', 'implementation', 'pretest',
                              'verification', 'research', 'content', 'operations')),
    ADD CONSTRAINT work_board_tickets_user_review_check
        CHECK (user_review IN ('none', 'approved', 'changes_requested')),
    ADD CONSTRAINT work_board_tickets_verification_verdict_check
        CHECK (verification_verdict IS NULL OR verification_verdict IN ('pass', 'fail')),
    ADD CONSTRAINT work_board_tickets_operation_impact_check
        CHECK (operation_impact IS NULL OR operation_impact IN ('internal', 'external')),
    ADD CONSTRAINT work_board_tickets_target_check
        CHECK (target_ticket_id IS NULL OR target_ticket_id <> id);

CREATE INDEX IF NOT EXISTS work_board_tickets_target_idx
    ON work_board_tickets (target_ticket_id) WHERE target_ticket_id IS NOT NULL;
