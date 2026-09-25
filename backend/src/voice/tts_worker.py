# Qwen3-TTS(MLX)를 한 번만 적재해 두고 표준입력 JSON 한 줄마다 문장 하나를 wav로 합성하는 상주 작업자다.
import json
import os
import sys
import time
import wave

import numpy as np
from mlx_audio.tts.utils import load_model

MODEL = os.environ.get("LOCAL_VOICE_MODEL", "mlx-community/Qwen3-TTS-12Hz-1.7B-CustomVoice-6bit")


def reply(payload):
    sys.stdout.write(json.dumps(payload, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def write_wav(path, audio, sample_rate):
    with wave.open(path, "wb") as out:
        out.setnchannels(1)
        out.setsampwidth(2)
        out.setframerate(sample_rate)
        out.writeframes((np.clip(audio, -1, 1) * 32767).astype("<i2").tobytes())


def synthesize(model, text, voice, instruct, ref_audio=None, ref_text=None):
    if ref_audio:
        # 기본(Base) 모델은 기준 음성의 목소리와 말투를 그대로 복제한다. 말투 지시는 받지 않는다.
        results = model.generate(text=text, ref_audio=ref_audio, ref_text=ref_text, lang_code="korean")
    else:
        results = model.generate(text=text, voice=voice, instruct=instruct or None, lang_code="korean")
    return np.concatenate([np.array(result.audio) for result in results])


def main():
    started = time.time()
    model = load_model(MODEL)
    # 첫 합성은 그래프 준비로 느리므로 호출 쪽이 직원 설정으로 한 번 예열 요청을 보낸다.
    reply({"ready": True, "model": MODEL, "loadMs": round((time.time() - started) * 1000),
           "sampleRate": model.sample_rate})
    for line in sys.stdin:
        if not line.strip():
            continue
        request = json.loads(line)
        begun = time.time()
        try:
            audio = synthesize(model, request["text"], request.get("voice"), request.get("instruct"),
                               request.get("refAudio"), request.get("refText"))
            # 재생 배율을 표본율에 곱하면 높낮이와 속도가 함께 올라간다(1.1이면 약 1.6반음·10%).
            rate = float(request.get("rate") or 1.0)
            write_wav(request["out"], audio, round(model.sample_rate * rate))
            reply({"id": request["id"], "path": request["out"], "synthMs": round((time.time() - begun) * 1000),
                   "audioMs": round(len(audio) / model.sample_rate / rate * 1000)})
        except Exception as error:  # 한 문장 실패로 작업자 전체를 끝내지 않는다.
            reply({"id": request.get("id"), "error": f"{type(error).__name__}: {error}"})


if __name__ == "__main__":
    main()
