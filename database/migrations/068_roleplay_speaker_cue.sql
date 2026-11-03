-- 프리셋에 화자 표시가 없어도 대사 앞에 화자 이름표를 쓰게 하는 앱 설정을 저장한다.
ALTER TABLE roleplay_profiles
  ADD COLUMN IF NOT EXISTS speaker_cue boolean NOT NULL DEFAULT false;
