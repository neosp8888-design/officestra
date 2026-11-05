-- 이 파일은 실행 중 업무에 끼워 넣은 사용자 입력(끼어들기)을 업무 활동으로 저장하게 한다.

ALTER TABLE turn_activities
DROP CONSTRAINT IF EXISTS turn_activities_kind_check;

ALTER TABLE turn_activities
ADD CONSTRAINT turn_activities_kind_check
CHECK (
    kind IN (
        'thinking',
        'command',
        'tool',
        'message',
        'collaboration',
        'suggestion',
        'steer'
    )
);
