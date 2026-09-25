-- 관리 보드의 프로젝트와 티켓은 저장소별 업무 기록 projects와 별개다.
CREATE TABLE IF NOT EXISTS work_board_projects (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    project_key text NOT NULL UNIQUE,
    title text NOT NULL CHECK (char_length(btrim(title)) BETWEEN 1 AND 120),
    description text NOT NULL DEFAULT '',
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT work_board_projects_key_check CHECK (project_key ~ '^[a-z0-9-]{2,64}$')
);

CREATE TABLE IF NOT EXISTS work_board_tickets (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id uuid NOT NULL REFERENCES work_board_projects(id) ON DELETE RESTRICT,
    title text NOT NULL CHECK (char_length(btrim(title)) BETWEEN 1 AND 160),
    description text NOT NULL DEFAULT '',
    assignee_id text REFERENCES characters(id) ON DELETE SET NULL,
    state text NOT NULL DEFAULT 'todo' CHECK (state IN ('todo', 'in_progress', 'blocked', 'done')),
    due_date date,
    completion_criteria text NOT NULL DEFAULT '',
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS work_board_tickets_project_state_idx
    ON work_board_tickets (project_id, state, created_at);

-- 사용자가 지정한 세 프로젝트만 만든다. 상태, 기한, 완료율, 티켓은 추정하지 않는다.
INSERT INTO work_board_projects (project_key, title) VALUES
    ('toss-trading', 'Toss 주식·트레이딩'),
    ('office-optimization', '오피스 최적화'),
    ('tts-settings', 'TTS 설정')
ON CONFLICT (project_key) DO NOTHING;
