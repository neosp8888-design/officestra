-- 업무 CLI와 별개인 롤플레이 대화: 가져온 리스 프리셋·모듈, 직원별 설정, 대화 기록.
CREATE TABLE IF NOT EXISTS roleplay_assets (
  id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  kind text NOT NULL CHECK (kind IN ('preset', 'module')),
  name text NOT NULL,
  source_sha256 text NOT NULL UNIQUE,
  data jsonb NOT NULL,
  imported_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS roleplay_chats (
  id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  character_id text NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
  created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS roleplay_chats_character_idx
  ON roleplay_chats (character_id, created_at DESC);

CREATE TABLE IF NOT EXISTS roleplay_profiles (
  character_id text PRIMARY KEY REFERENCES characters(id) ON DELETE CASCADE,
  enabled boolean NOT NULL DEFAULT false,
  preset_id uuid REFERENCES roleplay_assets(id) ON DELETE SET NULL,
  module_ids uuid[] NOT NULL DEFAULT '{}',
  toggles jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(toggles) = 'object'),
  character_description text NOT NULL DEFAULT '',
  first_message text NOT NULL DEFAULT '',
  user_name text NOT NULL DEFAULT '사용자',
  persona text NOT NULL DEFAULT '',
  model text NOT NULL DEFAULT 'gemini-3.8-flash-high',
  current_chat_id uuid REFERENCES roleplay_chats(id) ON DELETE SET NULL,
  updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS roleplay_messages (
  id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  chat_id uuid NOT NULL REFERENCES roleplay_chats(id) ON DELETE CASCADE,
  -- 같은 시각에 들어온 메시지도 넣은 순서대로 읽는다.
  position bigint GENERATED ALWAYS AS IDENTITY,
  role text NOT NULL CHECK (role IN ('user', 'char')),
  content text NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS roleplay_messages_chat_idx
  ON roleplay_messages (chat_id, position);
