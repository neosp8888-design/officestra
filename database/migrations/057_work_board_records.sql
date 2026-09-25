-- 업무 기록의 보존 상태는 티켓의 실행 상태와 별개다.
CREATE TABLE work_board_ticket_records (
    ticket_id uuid NOT NULL REFERENCES work_board_tickets(id) ON DELETE CASCADE,
    work_record_id uuid NOT NULL REFERENCES work_records(id) ON DELETE RESTRICT,
    linked_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (ticket_id, work_record_id)
);

CREATE INDEX work_board_ticket_records_record_idx
    ON work_board_ticket_records (work_record_id);
