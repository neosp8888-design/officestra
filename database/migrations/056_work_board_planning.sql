-- 2단계: 같은 티켓에 WBS 상하위·선행 관계와 기간 목표를 연결한다.
ALTER TABLE work_board_tickets
    ADD COLUMN parent_ticket_id uuid;

ALTER TABLE work_board_tickets
    ADD CONSTRAINT work_board_tickets_id_project_unique UNIQUE (id, project_id),
    ADD CONSTRAINT work_board_tickets_parent_same_project
        FOREIGN KEY (parent_ticket_id, project_id)
        REFERENCES work_board_tickets (id, project_id) ON DELETE RESTRICT,
    ADD CONSTRAINT work_board_tickets_no_self_parent
        CHECK (parent_ticket_id IS NULL OR parent_ticket_id <> id);

CREATE INDEX work_board_tickets_parent_idx
    ON work_board_tickets (parent_ticket_id);

CREATE TABLE work_board_ticket_dependencies (
    ticket_id uuid NOT NULL,
    predecessor_id uuid NOT NULL,
    project_id uuid NOT NULL,
    PRIMARY KEY (ticket_id, predecessor_id),
    CONSTRAINT work_board_dependency_ticket_fk
        FOREIGN KEY (ticket_id, project_id)
        REFERENCES work_board_tickets (id, project_id) ON DELETE CASCADE,
    CONSTRAINT work_board_dependency_predecessor_fk
        FOREIGN KEY (predecessor_id, project_id)
        REFERENCES work_board_tickets (id, project_id) ON DELETE RESTRICT,
    CONSTRAINT work_board_dependency_not_self
        CHECK (ticket_id <> predecessor_id)
);

CREATE INDEX work_board_ticket_dependencies_predecessor_idx
    ON work_board_ticket_dependencies (predecessor_id);

CREATE TABLE work_board_goals (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id uuid NOT NULL REFERENCES work_board_projects(id) ON DELETE RESTRICT,
    horizon text NOT NULL CHECK (horizon IN ('weekly', 'monthly')),
    period_start date NOT NULL,
    title text NOT NULL CHECK (char_length(btrim(title)) BETWEEN 1 AND 160),
    description text NOT NULL DEFAULT '',
    metric_name text,
    metric_unit text,
    target_value numeric(20,4),
    actual_value numeric(20,4),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT work_board_goals_period_alignment CHECK (
        (horizon = 'weekly' AND EXTRACT(ISODOW FROM period_start) = 1)
        OR (horizon = 'monthly' AND EXTRACT(DAY FROM period_start) = 1)
    ),
    CONSTRAINT work_board_goals_target_positive CHECK (
        target_value IS NULL OR target_value > 0
    ),
    CONSTRAINT work_board_goals_actual_nonnegative CHECK (
        actual_value IS NULL OR actual_value >= 0
    ),
    CONSTRAINT work_board_goals_metric_name_required CHECK (
        (target_value IS NULL AND actual_value IS NULL)
        OR (metric_name IS NOT NULL AND char_length(btrim(metric_name)) > 0)
    ),
    CONSTRAINT work_board_goals_id_project_unique UNIQUE (id, project_id)
);

CREATE INDEX work_board_goals_project_period_idx
    ON work_board_goals (project_id, period_start DESC, horizon);

CREATE TABLE work_board_goal_tickets (
    goal_id uuid NOT NULL,
    ticket_id uuid NOT NULL,
    project_id uuid NOT NULL,
    PRIMARY KEY (goal_id, ticket_id),
    CONSTRAINT work_board_goal_tickets_goal_fk
        FOREIGN KEY (goal_id, project_id)
        REFERENCES work_board_goals (id, project_id) ON DELETE CASCADE,
    CONSTRAINT work_board_goal_tickets_ticket_fk
        FOREIGN KEY (ticket_id, project_id)
        REFERENCES work_board_tickets (id, project_id) ON DELETE CASCADE
);

CREATE INDEX work_board_goal_tickets_ticket_idx
    ON work_board_goal_tickets (ticket_id);
