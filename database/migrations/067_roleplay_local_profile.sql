-- 롤플레이가 안티그래비티 프록시 대신 4090 로컬 모델 프로필로 답을 만들 수 있게 한다.
ALTER TABLE roleplay_profiles
  ADD COLUMN IF NOT EXISTS local_profile_id text
    REFERENCES local_agent_profiles(id) ON DELETE SET NULL;
