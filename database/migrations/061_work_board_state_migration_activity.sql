-- 기존 완료 판정을 검토로 되돌린 사유를 티켓 변경 이력에도 남긴다.
INSERT INTO work_board_ticket_activity (ticket_id, kind, changes)
SELECT ticket.id, 'changed',
       jsonb_build_object('state', jsonb_build_object('before', 'done', 'after', 'review'))
FROM work_board_tickets AS ticket
JOIN work_board_projects AS project ON project.id = ticket.project_id
WHERE project.project_key <> 'toss-trading'
  AND ticket.state = 'review'
  AND ticket.pushed_commit_sha IS NULL
  AND ticket.updated_at <= (
    SELECT applied_at FROM schema_migrations WHERE name = '060_work_board_delivery_states.sql'
  );
