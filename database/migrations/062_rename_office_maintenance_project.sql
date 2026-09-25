-- 기존 사용자가 붙인 다른 이름은 보존하고, 옛 기본 이름만 새 이름으로 바꾼다.
UPDATE work_board_projects
SET title = '오피스 유지보수', updated_at = now()
WHERE project_key = 'office-optimization'
  AND title = '오피스 최적화';
