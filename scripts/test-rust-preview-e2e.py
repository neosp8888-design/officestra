#!/usr/bin/env python3
"""Fresh Docker database → packaged Rust server → fake CLI → persisted turn and metadata.

Never connects to the operating database, real employee CLIs, Slack or Telegram.
"""
import argparse
import json
import os
from pathlib import Path
import plistlib
import shutil
import socket
import subprocess
import tempfile
import time
import urllib.request

ROOT = Path(__file__).resolve().parent.parent


def request(base, path, body=None, method=None):
    req = urllib.request.Request(base+path, data=json.dumps(body).encode() if body is not None else None,
                                 method=method, headers={'Origin': base, 'Content-Type': 'application/json'})
    with urllib.request.urlopen(req, timeout=10) as response:
        return response.status, json.load(response)


def await_value(get, accept, timeout=60):
    deadline = time.monotonic()+timeout
    last = None
    while time.monotonic() < deadline:
        try:
            last = get()
            if accept(last):
                return last
        except (OSError, ValueError):
            pass
        time.sleep(.25)
    raise RuntimeError('Isolated E2E did not reach expected state')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('app', nargs='?')
    args = parser.parse_args()
    backend_port = int(os.environ.get('OFFICESTRA_PACKAGED_E2E_BACKEND_PORT', '4319'))
    postgres_port = int(os.environ.get('OFFICESTRA_PACKAGED_E2E_POSTGRES_PORT', '55430'))
    if backend_port == 4317 or postgres_port == 54329:
        raise RuntimeError('Refusing operating service ports')
    for port in [backend_port, postgres_port]:
        with socket.socket() as listener:
            listener.bind(('127.0.0.1', port))
    with tempfile.TemporaryDirectory(prefix='officestra-rust-e2e-') as temporary:
        scratch = Path(temporary)
        if args.app:
            app = Path(args.app).resolve()
            if app.suffix == '.zip':
                subprocess.run(['ditto','-x','-k',str(app),str(scratch/'unzip')],check=True)
                app = scratch/'unzip/OFFICESTRA.app'
            runtime = app/'Contents/Resources/OFFICESTRARuntime'
            source_config = app/'Contents/Resources/OfficeLLM_OfficeCore.bundle/characters.json'
            release_id = plistlib.loads((app/'Contents/Info.plist').read_bytes())['OFFICESTRABackendReleaseID']
            subprocess.run(['python3',str(ROOT/'scripts/backend-release-id.py'),'--verify','--verify-rust-signature',str(app)],check=True)
            binary = runtime/'rust/bin/officestra-backend'
            native = binary.parent
        else:
            runtime = ROOT
            source_config = ROOT/'Sources/OfficeCore/Resources/characters.json'
            release_id = 'rust-source-e2e'
            native = ROOT/'backend-rs/target/release'
            binary = native/'officestra-backend'
        base = 'http://127.0.0.1:'+str(backend_port)
        compose = ['docker','compose','--project-name','officestra-r26-'+str(os.getpid()),
                   '-f',str(runtime/'infra/compose.yaml')]
        compose_env = {**os.environ, 'OFFICESTRA_POSTGRES_PORT':str(postgres_port)}
        env = compose_env.copy()
        child = None
        started = False
        try:
            home = scratch/'home'
            workspace = scratch/'project'
            home.mkdir(); workspace.mkdir()
            stale = scratch/'bin/codex'
            synced = scratch/'synchronized-bin/codex'
            for path, fixture in [(stale,'fake-codex-stale.sh'),(synced,'fake-codex.sh')]:
                path.parent.mkdir()
                shutil.copy2(ROOT/'scripts/fixtures'/fixture,path)
                path.chmod(0o700)
            config = json.loads(source_config.read_text())
            config.update(workdir=str(workspace),databaseBaseURL=base)
            for character in config['characters']:
                character.update(backend='codex',model='gpt-5.6-terra',effort='high',fastMode=False,
                                 permission='workspace-write',executablePath=str(stale))
            config_path = scratch/'characters.json'
            config_path.write_text(json.dumps(config)); config_path.chmod(0o600)
            log_path = scratch/'backend.log'
            env.update(HOME=str(home),DATABASE_URL='postgres://office:office-local@127.0.0.1:'+str(postgres_port)+'/office',
                       OFFICE_BACKEND_PORT=str(backend_port),OFFICE_WORKDIR=str(workspace),
                       CHARACTER_CONFIG_PATH=str(config_path),OFFICESTRA_RELEASE_ID=release_id,
                       OFFICE_MIGRATIONS_DIR=str(runtime/'database/migrations'),OFFICE_RESULT_TOOL_DIR=str(native),
                       OFFICE_SLACK_ENV_FILE=str(scratch/'no-slack.env'),SLACK_BOT_TOKEN='',SLACK_APP_TOKEN='',
                       OFFICE_SLACK_ALLOWED_USER_IDS='',OFFICESTRA_STALE_CLI_MARKER=str(scratch/'stale-used'))
            env.pop('OFFICE_NODE_EXECUTABLE',None)
            env.setdefault('OFFICE_EMBEDDING_MODEL_DIR', str(
                Path.home() / '.officestra/models/bge-m3-onnx-25b9af8e87a3'))
            env['OFFICE_EMBEDDING_ALLOW_MODEL_DOWNLOAD'] = '0'
            started = True
            subprocess.run(compose+['up','-d','--wait','--wait-timeout','120'],env=compose_env,check=True,stdout=subprocess.DEVNULL)
            with log_path.open('w') as log:
                child = subprocess.Popen([str(binary)],cwd=workspace,env=env,stdout=log,stderr=log)
                health = await_value(lambda:request(base,'/health')[1],
                                     lambda v:v.get('ok') and v.get('database',{}).get('ok') and v.get('releaseID')==release_id and v.get('idle'))
                assert health['activeTurnCount']==0
                _,sync=request(base,'/api/runtime/cli-paths',{'executables':{'codex':str(synced)}},'PUT')
                assert sync['ok'] and 'boss' in sync['updatedCharacterIds']
                _,job=request(base,'/api/agent-jobs',{'characterId':'boss','prompt':'Isolated fake CLI E2E; no external services.'})
                assert job['status']=='running'
                turn=await_value(lambda:request(base,'/api/live-feed/'+job['turnId'])[1]['turn'],lambda v:v['status']=='completed')
                assert 'Community preview first task completed.' in turn['response']
                assert not (scratch/'stale-used').exists()
                assert turn.get('sources'), 'Native result metadata was not saved with the turn'
                assert turn['sources'][0]['title']=='Rust native E2E'
                status,project=request(base,'/api/work-board/projects',{'projectKey':'rust-e2e','title':'Rust E2E'})
                assert status==201
                if 'project' in project: project=project['project']
                project=project['id']
                status,ticket=request(base,'/api/work-board/tickets',{'projectId':project,'title':'Rust E2E ticket'})
                assert status==201
                if 'ticket' in ticket: ticket=ticket['ticket']
                status,_=request(base,'/api/work-board/tickets/'+ticket['id'],{'title':'Rust E2E updated'},'PATCH')
                assert status==200
                status,_=request(base,'/api/work-board/tickets/'+ticket['id']+'/activity',{'kind':'verification','body':'isolated Rust write path'})
                assert status==201
                print('Rust preview E2E passed: fresh isolated DB, migrations, health, CLI path sync, turn, native metadata, board writes')
        except Exception:
            # Do not dump logs: a failed service can include a database URL.
            if 'log_path' in locals() and log_path.exists():
                print('Isolated backend log retained in error artifact without credentials')
                safe=log_path.read_text(errors='replace').replace(env.get('DATABASE_URL','__none__'),'<isolated database>')
                (ROOT/'dist/r26-e2e-failure.log').write_text(safe)
            raise
        finally:
            if child is not None:
                child.terminate()
                try: child.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    child.kill(); child.wait()
            if started:
                subprocess.run(compose+['down','-v','--remove-orphans'],env=compose_env,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)


if __name__=='__main__':
    main()
