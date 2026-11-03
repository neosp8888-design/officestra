#!/usr/bin/env python3
"""Install into the single normal app path; never stop/start an app or backend."""
import argparse
import datetime
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_APP = ROOT / 'dist/OFFICESTRA.app'
BACKUPS = Path.home() / 'Library/Application Support/OFFICESTRA/Backups/apps'


def bundle_in_use(app):
    processes = subprocess.check_output(['ps', '-axo', 'comm='], text=True)
    prefix = str(app.resolve()) + '/'
    return any(line.strip().startswith(prefix) for line in processes.splitlines())


def verify(app):
    subprocess.run(['codesign', '--verify', '--deep', '--strict', str(app)], check=True, capture_output=True)
    subprocess.run(['python3', str(ROOT/'scripts/backend-release-id.py'), '--verify',
                    '--verify-rust-signature', str(app)], check=True, stdout=subprocess.DEVNULL)


def archive_bundle(app, archive):
    subprocess.run(['ditto', '-c', '-k', '--sequesterRsrc', '--keepParent', str(app), str(archive)],
                   check=True, capture_output=True)
    if not archive.is_file() or archive.stat().st_size == 0:
        raise RuntimeError('Rollback archive was not created')


def install(source, destination=DEFAULT_APP, backups=BACKUPS, *, allow_running=False):
    source, destination = source.resolve(), destination.resolve()
    if source == destination or destination in source.parents or source in destination.parents:
        raise RuntimeError('Source and destination must be separate app bundles')
    if not source.is_dir():
        raise RuntimeError('Candidate app is missing')
    if not allow_running and (bundle_in_use(destination) or bundle_in_use(source)):
        raise RuntimeError('App or backend is still running; no files were replaced')
    verify(source)
    backups.mkdir(parents=True, exist_ok=True)
    destination.parent.mkdir(parents=True, exist_ok=True)
    stamp = datetime.datetime.now().strftime('%Y%m%dT%H%M%S%f')
    archive = backups / ('OFFICESTRA-' + stamp + '.zip')
    if destination.exists():
        archive_bundle(destination, archive)
    # Archiving can take time. Refuse a user relaunch before making any change.
    if not allow_running and (bundle_in_use(destination) or bundle_in_use(source)):
        raise RuntimeError('App was reopened during preparation; no files were replaced')
    staging = Path(tempfile.mkdtemp(prefix='.app-install-', dir=destination.parent))
    previous = staging / 'OFFICESTRA.app'
    promoted = False
    try:
        had_previous = destination.exists()
        if had_previous:
            destination.rename(previous)
        try:
            source.rename(destination)
            promoted = True
        except Exception:
            if had_previous:
                previous.rename(destination)
            raise
    finally:
        # Keep the original bundle if even rollback failed; the ZIP is an additional copy.
        if promoted or not previous.exists():
            shutil.rmtree(staging)
    # The old file IDs disappear here; Dock cannot follow an old bundle to a backup app.
    return archive if archive.exists() else None


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('source', type=Path)
    parser.add_argument('--destination', type=Path, default=DEFAULT_APP)
    parser.add_argument('--replace-running', action='store_true', help='Explicit user-directed bundle build; no process restart')
    args = parser.parse_args()
    archive = install(args.source, args.destination, allow_running=args.replace_running)
    print('Installed: ' + str(args.destination))
    if archive:
        print('Rollback archive: ' + str(archive))


if __name__ == '__main__':
    main()
