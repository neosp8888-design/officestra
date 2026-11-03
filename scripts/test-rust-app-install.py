#!/usr/bin/env python3
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch
import rust_app_install as installer


class AppInstallTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.source = self.root/'candidate.app'
        self.target = self.root/'OFFICESTRA.app'
        self.backups = self.root/'backups'
        for app, version in [(self.source, 'new'), (self.target, 'old')]:
            app.mkdir(); (app/'version').write_text(version)
        self.addCleanup(patch.stopall)
        patch.object(installer, 'verify').start()
        patch.object(installer, 'archive_bundle', side_effect=lambda app, out: out.write_text((app/'version').read_text())).start()

    def test_running_app_or_backend_blocks_every_write(self):
        with patch.object(installer, 'bundle_in_use', return_value=True):
            with self.assertRaises(RuntimeError):
                installer.install(self.source, self.target, self.backups)
        self.assertEqual((self.target/'version').read_text(), 'old')
        self.assertFalse(self.backups.exists())

    def test_reopened_app_after_archive_blocks_replacement(self):
        with patch.object(installer, 'bundle_in_use', side_effect=[False, False, True]):
            with self.assertRaises(RuntimeError):
                installer.install(self.source, self.target, self.backups)
        self.assertEqual((self.target/'version').read_text(), 'old')
        self.assertTrue(self.source.exists())

    def test_explicit_build_can_replace_bundle_without_process_commands(self):
        with patch.object(installer, 'bundle_in_use', return_value=True), patch.object(installer.subprocess, 'run') as run:
            archive = installer.install(self.source, self.target, self.backups, allow_running=True)
        run.assert_not_called()
        self.assertEqual(archive.read_text(), 'old')
        self.assertEqual((self.target/'version').read_text(), 'new')

    def test_install_preserves_old_archive_and_leaves_one_app(self):
        with patch.object(installer, 'bundle_in_use', return_value=False):
            archive = installer.install(self.source, self.target, self.backups)
        self.assertEqual(archive.read_text(), 'old')
        self.assertEqual((self.target/'version').read_text(), 'new')
        self.assertFalse(self.source.exists())

    def test_failed_promotion_restores_previous_app(self):
        rename = Path.rename
        def fail_new(path, target):
            if path == self.source:
                raise OSError('simulated promotion failure')
            return rename(path, target)
        with patch.object(installer, 'bundle_in_use', return_value=False), patch.object(Path, 'rename', fail_new):
            with self.assertRaises(OSError):
                installer.install(self.source, self.target, self.backups)
        self.assertEqual((self.target/'version').read_text(), 'old')
        self.assertTrue(self.source.exists())


    def test_failed_rollback_retains_original_bundle_for_recovery(self):
        rename = Path.rename
        def fail_new_and_restore(path, target):
            if path == self.source or path.parent.name.startswith('.app-install-'):
                raise OSError('simulated filesystem failure')
            return rename(path, target)
        with patch.object(installer, 'bundle_in_use', return_value=False), patch.object(Path, 'rename', fail_new_and_restore):
            with self.assertRaises(OSError):
                installer.install(self.source, self.target, self.backups)
        retained = list(self.root.glob('.app-install-*/OFFICESTRA.app/version'))
        self.assertEqual([p.read_text() for p in retained], ['old'])


if __name__ == '__main__':
    unittest.main()
