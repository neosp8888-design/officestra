#!/usr/bin/env python3
"""Exercise the shipped native helpers and embedding worker without starting a server."""
import json
import math
import os
from pathlib import Path
import subprocess
import sys
import tempfile


def check_runtime(runtime, *, embedding=True):
    runtime = Path(runtime)
    if (runtime / 'node').exists() or (runtime / 'backend').exists():
        raise RuntimeError('Node runtime or backend found in Rust-only bundle')
    native = runtime / 'rust/bin'
    for name in ['officestra-backend', 'officestra-result', 'officestra-terminal-hook',
                 'officestra-voice-follow', 'officestra-telegram']:
        if not os.access(native / name, os.X_OK):
            raise RuntimeError('Missing native executable: ' + name)
    subprocess.run([str(native / 'officestra-result'), '--help'], check=True, stdout=subprocess.DEVNULL)
    with tempfile.TemporaryDirectory(prefix='native-result-smoke-') as temporary:
        result = Path(temporary) / 'result.json'
        result.write_text(json.dumps(dict(version=1, sourcesSubmitted=False,
                                          wikiProposalsSubmitted=False, sources=[], wikiProposals=[])))
        result.chmod(0o600)
        environment = {**os.environ, 'OFFICESTRA_RESULT_PATH': str(result)}
        subprocess.run([str(native / 'officestra-result'), 'source', '--kind', 'file', '--title', 'smoke',
                        '--locator', 'native-smoke'], env=environment, check=True, stdout=subprocess.DEVNULL)
        reply = json.loads(subprocess.check_output([str(native / 'officestra-result'), 'show'], env=environment))
        if reply['sources'][0]['locator'] != 'native-smoke':
            raise RuntimeError('Native result CLI did not persist metadata')
    if embedding:
        environment = {**os.environ, 'OFFICE_EMBEDDING_ALLOW_MODEL_DOWNLOAD': '0'}
        environment.setdefault('OFFICE_EMBEDDING_MODEL_DIR', str(
            Path.home() / '.officestra/models/bge-m3-onnx-25b9af8e87a3'))
        environment.pop('ORT_DYLIB_PATH', None)
        request = dict(id=1, texts=['OFFICESTRA Rust 패키지 검증', 'OFFICESTRA Rust 패키지 검증', '바다를 헤엄치는 물고기'])
        worker = subprocess.run([str(native / 'officestra-backend'), 'embedding-worker'],
                                input=json.dumps(request)+'\n', text=True, capture_output=True,
                                env=environment, timeout=180)
        if worker.returncode != 0:
            raise RuntimeError('Embedding worker exited: ' + worker.stderr.strip())
        response = json.loads(worker.stdout.splitlines()[0])
        if response.get('ok') is not True:
            raise RuntimeError('Embedding worker failed: ' + str(response.get('error')))
        vectors = response['result']['vectors']
        if len(vectors) != 3 or any(len(v) != 1024 or not all(math.isfinite(n) for n in v) for v in vectors):
            raise RuntimeError('Invalid embedding shape or non-finite coordinates')
        if any(abs(math.sqrt(sum(n*n for n in v))-1) > .01 for v in vectors):
            raise RuntimeError('Embedding vectors are not normalized')
        if sum(a*b for a,b in zip(vectors[0], vectors[1])) < .999:
            raise RuntimeError('Identical text produced different vectors')
        if sum(a*b for a,b in zip(vectors[0], vectors[2])) > .999:
            raise RuntimeError('Distinct text produced identical vectors')
    print('Rust-only runtime smoke passed: native metadata CLI and 1024D embeddings')


if __name__ == '__main__':
    check_runtime(Path(sys.argv[1]))
