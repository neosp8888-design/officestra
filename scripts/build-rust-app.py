#!/usr/bin/env python3
"""Build a Rust-only app. No Node runtime or Node backend is shipped."""
import hashlib
import json
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import tempfile
from rust_app_install import bundle_in_use, install

ROOT = Path(__file__).resolve().parent.parent
RUST = ROOT / 'backend-rs'
BINS = ('officestra-backend', 'officestra-result', 'officestra-terminal-hook',
        'officestra-voice-follow', 'officestra-telegram')


def run(*args, **kwargs):
    return subprocess.run([str(a) for a in args], check=True, **kwargs)


def output(*args, **kwargs):
    return subprocess.check_output([str(a) for a in args], text=True, **kwargs).strip()


def copy_tree(source, destination):
    shutil.copytree(source, destination, dirs_exist_ok=True,
                    ignore=shutil.ignore_patterns('__pycache__', '.DS_Store'))


def resource_bundle(binary_directory, name):
    bundle = binary_directory / name
    if (bundle / 'Contents/Resources').is_dir():
        return bundle / 'Contents/Resources', bundle / 'Contents/Info.plist'
    return bundle, bundle / 'Info.plist'


def sign(path, identity):
    args = ['codesign', '--force', '--sign', identity]
    if identity != '-':
        args += ['--options', 'runtime', '--timestamp']
    run(*args, path)


def notices(cargo, licenses):
    """Keep the actual cached crate license texts alongside their SPDX expressions."""
    metadata = json.loads(output(cargo, 'metadata', '--offline', '--locked',
                                 '--filter-platform', 'aarch64-apple-darwin',
                                 '--format-version', '1', cwd=RUST))
    lines = ['# OFFICESTRA Rust runtime third-party notices', '',
             'The entries below describe build dependencies; not every crate is linked on macOS.', '']
    for package in sorted(metadata['packages'], key=lambda p: (p['name'], p['version'])):
        if package['source'] is None:
            continue
        name = package['name'] + '-' + package['version']
        lines += ['## ' + name, '', 'License: ' + (package['license'] or 'see included license files'), '']
        directory = Path(package['manifest_path']).parent
        candidates = [p for p in directory.iterdir() if p.is_file()
                      and p.name.lower().startswith(('license', 'licence', 'copying', 'notice'))]
        if package.get('license_file'):
            candidates.append(directory / package['license_file'])
        for source in sorted(set(candidates)):
            if source.is_file():
                destination = licenses / 'rust-crates' / name / source.name
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(source, destination)
                lines += ['- rust-crates/' + name + '/' + source.name]
        lines += ['']
    lines += ['## ONNX Runtime', '', 'MIT; see ONNXRuntime-LICENSE.', '',
              '## Antigravity Playwright driver', '',
              'Apache-2.0; license and third-party notices are included in tools/antigravity-playwright/package.',
              'This is an external CLI adapter. It uses the Node executable installed with the user’s CLI tools.',
              'OFFICESTRA itself ships neither Node nor a JavaScript backend.', '']
    (licenses / 'THIRD-PARTY-NOTICES.md').write_text('\n'.join(lines))


def main():
    if output('uname', '-m') != 'arm64':
        raise RuntimeError('OFFICESTRA requires Apple Silicon arm64')
    destination = Path(os.environ.get('OFFICESTRA_APP_BUNDLE', str(ROOT / 'dist/OFFICESTRA.app'))).absolute()
    replace_running = os.environ.get('OFFICESTRA_REPLACE_RUNNING_APP') == '1'
    if bundle_in_use(destination) and not replace_running:
        raise RuntimeError('실행 중인 앱과 백엔드를 먼저 앱 버튼으로 종료하세요. 운영 경로는 dist/OFFICESTRA.app 하나입니다.')
    listener = subprocess.run(['lsof', '-t', '-iTCP:4317', '-sTCP:LISTEN'], capture_output=True, text=True)
    for pid in listener.stdout.split():
        executable = output('ps', '-p', pid, '-o', 'comm=')
        if executable.startswith(str(destination) + '/') and not replace_running:
            raise RuntimeError('실행 중인 앱은 교체하지 않습니다. OFFICESTRA_APP_BUNDLE로 검증용 출력 경로를 지정하세요.')
    cargo = shutil.which('cargo') or str(Path.home() / '.cargo/bin/cargo')
    env = {**os.environ, 'RUSTFLAGS': '--remap-path-prefix=' + str(Path.home()) + '=~'}
    print('Rust 실행 파일 5개 빌드', flush=True)
    run(cargo, 'build', '--release', '--locked', '--offline', '--bins', cwd=RUST, env=env)
    print('Swift 앱 release 빌드', flush=True)
    run('swift', 'build', '-c', 'release', '--product', 'OfficeLLM', cwd=ROOT)
    binary_directory = Path(output('swift', 'build', '-c', 'release', '--show-bin-path', cwd=ROOT))
    destination.parent.mkdir(parents=True, exist_ok=True)
    identity = os.environ.get('OFFICESTRA_CODESIGN_IDENTITY', '-')
    with tempfile.TemporaryDirectory(prefix='.rust-app-', dir=destination.parent) as temporary:
        app = Path(temporary) / 'OFFICESTRA.app'
        contents = app / 'Contents'
        resources = contents / 'Resources'
        runtime = resources / 'OFFICESTRARuntime'
        native = runtime / 'rust/bin'
        licenses = runtime / 'licenses'
        native.mkdir(parents=True)
        licenses.mkdir(parents=True)
        (contents / 'MacOS').mkdir()
        shutil.copy2(binary_directory / 'OfficeLLM', contents / 'MacOS/OfficeLLM')
        shutil.copy2(ROOT / 'Resources/Info.plist', contents / 'Info.plist')
        copy_tree(ROOT / 'Sources/OfficeGame/Resources', resources)
        shutil.copy2(ROOT / 'Resources/OFFICESTRA.icns', resources / 'OFFICESTRA.icns')
        for name in ['OfficeLLM_OfficeCore.bundle', 'OfficeLLM_OfficeGame.bundle']:
            source, info = resource_bundle(binary_directory, name)
            copy_tree(source, resources / name)
            shutil.copy2(info, resources / name / 'Info.plist')
        copy_tree(ROOT / 'database/migrations', runtime / 'database/migrations')
        (runtime / 'infra').mkdir()
        shutil.copy2(ROOT / 'infra/compose.yaml', runtime / 'infra/compose.yaml')
        for name in BINS:
            shutil.copy2(RUST / 'target/release' / name, native / name)
        # Existing vendor cache is a build input only. npm and Node are not executed or shipped.
        ort = Path(os.environ.get('OFFICESTRA_ORT_DYLIB', str(ROOT / 'backend/node_modules/onnxruntime-node/bin/napi-v6/darwin/arm64/libonnxruntime.1.24.3.dylib')))
        shutil.copy2(ort, native / 'libonnxruntime.dylib')
        ort_license = ROOT / 'Vendor/ONNXRuntime/LICENSE'
        shutil.copy2(ort_license, licenses / 'ONNXRuntime-LICENSE')
        copy_tree(ROOT / 'backend/src/playwright-driver-1.57.0', runtime / 'tools/antigravity-playwright')
        shutil.copy2(ROOT / 'LICENSE', licenses / 'OFFICESTRA-LICENSE')
        shutil.copy2(ROOT / 'Vendor/SwiftTerm/LICENSE', licenses / 'SwiftTerm-LICENSE')
        notices(cargo, licenses)
        configuration_path = resources / 'OfficeLLM_OfficeCore.bundle/characters.json'
        configuration = json.loads(configuration_path.read_text())
        workdir = os.environ.get('OFFICESTRA_WORKDIR', '/Users/your-name/Projects')
        if not Path(workdir).is_absolute():
            raise RuntimeError('업무 폴더가 절대 경로가 아닙니다.')
        configuration['workdir'] = workdir
        configuration_path.write_text(json.dumps(configuration, ensure_ascii=False, indent=2) + '\n')
        run('strip', '-S', contents / 'MacOS/OfficeLLM')
        for name in BINS:
            run('strip', '-S', native / name)
            (native / name).chmod(0o755)
            if output('lipo', '-archs', native / name) != 'arm64':
                raise RuntimeError('Rust executable is not arm64: ' + name)
            dependencies = output('otool', '-L', native / name).splitlines()[1:]
            if any(not line.strip().startswith(('/usr/lib/', '/System/Library/')) for line in dependencies):
                raise RuntimeError('External dynamic library dependency: ' + name)
        files = (*BINS, 'libonnxruntime.dylib')
        hashes = [hashlib.sha256((native / name).read_bytes()).hexdigest() + '  ' + name for name in files]
        (runtime / 'rust/SHA256').write_text('\n'.join(hashes) + '\n')
        cdhashes = []
        for name in files:
            sign(native / name, identity)
            details = run('codesign', '-d', '--verbose=4', native / name, capture_output=True, text=True)
            cdhash = next(line[7:] for line in details.stderr.splitlines() if line.startswith('CDHash='))
            cdhashes.append(cdhash + '  ' + name)
        (runtime / 'rust/CDHASH').write_text('\n'.join(cdhashes) + '\n')
        run('python3', ROOT / 'scripts/smoke-test-rust-runtime.py', runtime)
        release_id = output('python3', ROOT / 'scripts/backend-release-id.py', app)
        info_path = contents / 'Info.plist'
        info = plistlib.loads(info_path.read_bytes())
        info['OFFICESTRABackendReleaseID'] = release_id
        info_path.write_bytes(plistlib.dumps(info))
        sign(contents / 'MacOS/OfficeLLM', identity)
        sign(app, identity)
        run('codesign', '--verify', '--deep', '--strict', app)
        run('python3', ROOT / 'scripts/backend-release-id.py', '--verify', '--verify-rust-signature', app)
        # A ZIP outside dist preserves rollback without leaving another runnable app beside it.
        # Explicit user-directed build only replaces bundle files; processes are not restarted.
        install(app, destination, allow_running=replace_running)
    print('Rust 전용 앱: ' + str(destination), flush=True)


if __name__ == '__main__':
    main()
