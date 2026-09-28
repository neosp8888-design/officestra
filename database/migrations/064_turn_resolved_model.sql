-- model은 CLI에 넘긴 요청값(별칭 포함)으로 재실행에 쓰고, 실제로 응답한 모델 ID는 따로 남긴다.
ALTER TABLE turns ADD COLUMN IF NOT EXISTS resolved_model text;
