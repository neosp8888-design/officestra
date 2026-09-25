# 음성 지원 (응답 따라 읽기)

실시간 대화 제목 줄의 **음성 지원** 버튼을 누르면, 지금 선택한 직원의 응답이 스트리밍되는 동안 완성된 문장부터 맥에서 Qwen3-TTS로 합성해 바로 읽어 준다. 다시 누르면 끈다. 한 번에 한 직원만 읽으며, 다른 직원에서 켜면 이전 직원의 읽기는 멈춘다. 켠 뒤에 시작된 응답부터 읽고, 새 응답이 시작되면 읽던 응답은 끊는다.

켜짐 상태는 백엔드 프로세스에만 있다. 백엔드를 다시 시작하면 꺼진 상태로 돌아간다.
기존에 터미널에서 띄운 `work/local-voice/voice-follow.mjs`는 이 버튼의 제어 대상이 아니다. 두 실행 경로가 같은 잠금을 사용하므로, 이미 하나가 응답을 읽는 동안 다른 하나를 켜면 중복 재생 없이 실패 상태를 알린다. 수동 실행을 종료한 뒤 버튼으로 다시 켠다. 두 프로세스의 임시 wav 파일도 서로 다른 폴더에 둔다.

## 준비

합성은 맥에서 mlx-audio로 한다. 파이썬 환경은 앱 번들 밖 `~/.officestra/voice/.venv`에 둔다.
이 맥에는 기존 시제품에서 검증한 Python 3.12 환경을 이 위치로 복사해 두었고,
새 경로에서 `mlx_audio`를 가져오는 것까지 확인했다. 실행 중인 수동 시제품과는 별개 환경이다.

```sh
python3 -m venv ~/.officestra/voice/.venv
~/.officestra/voice/.venv/bin/pip install mlx-audio
```

모델은 처음 합성할 때 `~/.cache/huggingface`에 받는다. 환경이 없으면 버튼에 경고 표시가 뜨고, 마우스를 올리면 이유가 보인다. 경로는 `OFFICE_VOICE_DIR`(기본 `~/.officestra/voice`)과 `OFFICE_VOICE_PYTHON`으로 바꿀 수 있다.

## 동작

- 백엔드 API: `GET /api/voice-follow`는 `{characterId, state, error}`를 돌려준다. `state`는 `stopped`·`starting`·`running`·`failed` 중 하나다. `POST /api/voice-follow`에 `{enabled, characterId}`를 보내 켜고 끈다(`backend/src/voice-follow-control.mjs`).
- 켜면 백엔드가 `backend/src/voice/voice-follow.mjs`를 자식 프로세스로 띄운다. 자식은 `/ws` 변경 알림을 구독한다. 연결·재연결 때 한 번 동기화하고, 이후 대상 직원의 변경 알림이 있을 때만 `GET /api/voice-feed?characterId=…&since=…`로 최신 답변 하나를 읽는다. 작업 내역·비용·출처는 조회하지 않는다. 알림은 100ms 단위로 합치고 같은 응답은 다시 전달하지 않는다. 정상 연결의 대기 상태에서는 정기 조회가 없다. 오류 시에만 1~30초 간격으로 재시도하며 백엔드가 끝나면 자식도 함께 끝난다.
- 합성 작업자(`tts_worker.py`)는 모델을 한 번만 올리고 예열한 뒤 문장을 wav로 만든다. 재생은 `afplay`로 한다.
- 실행 기록은 `~/.officestra/voice/logs/follow.log`, 문장별 지연은 `latency.jsonl`에 남는다.

## 읽는 글

- `speakable.mjs`가 누적 응답에서 완성된 문장만 뽑는다. 코드 블록·표·수평선은 건너뛰고, 링크는 글자만, 경로는 파일 이름만 읽는다.
- 괄호 안의 글은 연기 지침으로 보고 읽지 않는다(`parseDirections`). 지침의 낱말로 `voices.json`의 `styles` 기준 음성을 고른다. 슬픔(울먹, 눈물 등)·아픔(아파, 고통 등)·분노(화가, 버럭 등)·질투(질투, 삐진 등)·흥분(들뜬, 신나 등)·속삭임(속삭, 귓가 등) 순서로 먼저 맞는 것을 쓴다. 장난 계열 말은 기준 음성이 없어 기본 목소리로 읽는다.
- 침묵·한숨·머뭇 같은 말이 있으면 문장 앞에서 0.7초 쉰다. 지침만 있는 조각이면 다음 문장 앞에서 쉰다.
- 말투는 새 지침이 나오거나 빈 줄로 문단이 끝날 때까지 이어진다.

## 목소리 설정

공개 저장소의 `backend/src/voice/voices.json`에는 일반 기본 목소리만 둔다. 개인 음성 설정은 저장소 밖 `~/.officestra/voice/voices.json`에 두며, 이 파일이 있으면 우선 사용한다. 상대 경로 `refAudio`는 설정 파일이 있는 폴더를 기준으로 해석한다. 없는 직원은 `default`(CustomVoice 모델의 `sohee` 화자)를 쓴다. 개인 기준 음성 WAV는 `~/.officestra/voice/refs/`에 두고 저장소에는 넣지 않는다.

- 로컬 설정에서 `refAudio`와 `refText`를 지정하면 기준 음성을 복제해 읽는다. 감정별 `styles`를 둔 경우 해당 기준 음성을 사용한다.

## 직접 실행

```sh
# 앱 없이 파일을 초당 30자로 흘려 확인하기. --no-play는 소리 없이 재생 길이만 기다린다.
node backend/src/voice/voice-follow.mjs --replay backend/test/fixtures/voice-sample-reply.md --cps 30 --no-play
# 합성 없이 읽을 문장만 확인하기
node backend/src/voice/voice-follow.mjs --replay backend/test/fixtures/voice-sample-reply.md --dry-run
# 가짜 피드로 따라 읽기 확인하기
node scripts/voice-mock-feed.mjs &
node backend/src/voice/voice-follow.mjs --backend http://127.0.0.1:47999
```
