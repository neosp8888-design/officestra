-- 티켓 설명을 덮어쓰지 않고 진행과 변경을 보존한다.
ALTER TABLE work_board_tickets
    ADD COLUMN decision_pending boolean NOT NULL DEFAULT false;

CREATE TABLE work_board_ticket_activity (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    ticket_id uuid NOT NULL REFERENCES work_board_tickets(id) ON DELETE CASCADE,
    kind text NOT NULL CHECK (kind IN ('created', 'changed', 'progress', 'decision', 'verification')),
    body text NOT NULL DEFAULT '' CHECK (char_length(body) <= 4000),
    changes jsonb NOT NULL DEFAULT '{}'::jsonb,
    reported_actor_id text,
    created_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT work_board_ticket_activity_changes_object CHECK (jsonb_typeof(changes) = 'object')
);

CREATE INDEX work_board_ticket_activity_ticket_time_idx
    ON work_board_ticket_activity (ticket_id, created_at DESC, id DESC);
