#!/usr/bin/env python3
"""Prepare the stable native Telegram helper and launchd file without restarting it."""
import argparse
import datetime
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import tempfile


def install(app, home):
    source = app/'Contents/Resources/OFFICESTRARuntime/rust/bin/officestra-telegram'
    subprocess.run(['codesign','--verify','--strict',str(source)],check=True,capture_output=True)
    target = home/'Library/Application Support/OFFICESTRA/bin/officestra-telegram'
    target.parent.mkdir(parents=True,exist_ok=True)
    if target.exists():
        backup=target.parent/'previous'/('officestra-telegram-'+datetime.datetime.now().strftime('%Y%m%dT%H%M%S'))
        backup.parent.mkdir(parents=True,exist_ok=True)
        shutil.copy2(target,backup)
    with tempfile.NamedTemporaryFile(dir=target.parent,delete=False) as temporary:
        staged=Path(temporary.name)
    try:
        shutil.copy2(source,staged)
        staged.chmod(0o755)
        os.replace(staged,target)
    finally:
        staged.unlink(missing_ok=True)
    logs=home/'Library/Logs'
    logs.mkdir(parents=True,exist_ok=True)
    data={'Label':'com.neo.officestra-telegram','ProgramArguments':[str(target)],
          'RunAtLoad':True,'KeepAlive':{'SuccessfulExit':False},'ThrottleInterval':20,
          'StandardOutPath':str(logs/'officestra-telegram.log'),
          'StandardErrorPath':str(logs/'officestra-telegram.error.log')}
    plist=home/'Library/LaunchAgents/com.neo.officestra-telegram.plist'
    plist.parent.mkdir(parents=True,exist_ok=True)
    if plist.exists():
        shutil.copy2(plist,plist.with_suffix('.plist.r26-backup'))
    with tempfile.NamedTemporaryFile(dir=plist.parent,delete=False) as temporary:
        staged=Path(temporary.name)
        temporary.write(plistlib.dumps(data))
    try:
        staged.chmod(0o600)
        os.replace(staged,plist)
    finally:
        staged.unlink(missing_ok=True)
    print('Rust Telegram helper and launchd file prepared; current process was not restarted.')


if __name__=='__main__':
    parser=argparse.ArgumentParser()
    parser.add_argument('app',type=Path)
    parser.add_argument('--home',type=Path,default=Path.home())
    args=parser.parse_args()
    install(args.app.resolve(),args.home)
