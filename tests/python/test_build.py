"""Check build caching and portable runtime boundaries."""
import json
import hashlib
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import build_runtime
import verify_portable_host
import host_abi
import build as launcher_build
import build_venus_probe


class BuildTests(unittest.TestCase):
    def test_renderer_patch_is_applied_once_and_does_not_skip_an_unpatched_tree(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root/'sample.c'
            source.write_text('before\n')
            patch_file = root/'change.patch'
            patch_file.write_text('--- a/sample.c\n+++ b/sample.c\n@@ -1 +1 @@\n-before\n+after\n')
            build_venus_probe.apply_renderer_patch(root, patch_file)
            self.assertEqual(source.read_text(), 'after\n')
            build_venus_probe.apply_renderer_patch(root, patch_file)
            self.assertEqual(source.read_text(), 'after\n')

    def test_default_build_needs_no_proton_installation_or_repair(self):
        with tempfile.TemporaryDirectory() as directory:
            with patch('sys.argv', ['build.py', '--engine', 'docker', '--output', directory]), \
                    patch('build.source_identity', return_value='source'), \
                    patch('build.subprocess.check_output', return_value='image\n'), \
                    patch('build.subprocess.run') as run:
                launcher_build.main()
            command = run.call_args.args[0]
            self.assertIn('LEGION_WINE_REPAIR=0', command)
            self.assertFalse(any(':/proton:' in arg for arg in command))

    def test_optional_dependency_input_is_not_subject_to_the_repair_hash_pin(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            proton = root/'other-proton'
            proton.mkdir()
            with patch('sys.argv', ['build.py', '--engine', 'docker', '--output', str(root/'build'), '--proton', str(proton)]), \
                    patch('build.source_identity', return_value='source'), \
                    patch('build.subprocess.check_output', return_value='image\n'), \
                    patch('build.subprocess.run') as run:
                launcher_build.main()
            self.assertIn(str(proton)+':/proton:ro', run.call_args.args[0])

    def test_selinux_build_preserves_existing_volume_labels(self):
        with patch('build.subprocess.check_output', return_value='{"selinuxEnabled":true}'):
            self.assertEqual(launcher_build.container_security('podman'), ['--security-opt', 'label=disable'])
        with patch('build.subprocess.check_output', return_value='{"selinuxEnabled":false}'):
            self.assertEqual(launcher_build.container_security('podman'), [])
        with patch('build.subprocess.check_output') as info:
            self.assertEqual(launcher_build.container_security('docker'), [])
            info.assert_not_called()

    def test_build_identity_changes_when_source_is_modified(self):
        with patch('build.subprocess.check_output', side_effect=['commit\n', b'']):
            self.assertEqual(launcher_build.source_identity(Path('.')), 'commit')
        with patch('build.subprocess.check_output', side_effect=['commit\n', b'diff']):
            self.assertTrue(launcher_build.source_identity(Path('.')).startswith('commit-dirty-'))

    def test_host_abi_rejects_a_corrupt_cached_package_before_extraction(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            name = 'runtime-1-x86_64.pkg.tar.zst'
            (root/name).write_bytes(b'corrupt package')
            manifest = root/'packages.json'
            manifest.write_text(json.dumps({name: dict(
                url='https://example.org/'+name, sha256='a'*64
            )}))
            with patch('host_abi.subprocess.run') as run:
                with self.assertRaisesRegex(ValueError, 'checksum mismatch'):
                    host_abi.install(manifest, root, root/'lib', root/'licenses')
                run.assert_not_called()

    def test_failed_rebuild_cannot_reuse_an_old_success_marker(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with patch.object(build_runtime, 'OUT', root):
                build_runtime.step('component', ['first'], lambda: None)
                with self.assertRaisesRegex(RuntimeError, 'interrupted'):
                    build_runtime.step('component', ['second'],
                                       lambda: self.fail_build())
                self.assertFalse((root/'state/component.json').exists())

    @staticmethod
    def fail_build():
        raise RuntimeError('interrupted')

    def test_unchanged_build_skips_its_action(self):
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(build_runtime, 'OUT', Path(directory)):
                build_runtime.step('component', ['input'], lambda: None)
                build_runtime.step('component', ['input'],
                                   lambda: self.fail('cached action ran'))

    def test_portability_check_rejects_manifest_escape(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            assets = dict(qemu='../outside', host_loader='loader', host_libraries='lib')
            manifest = dict(assets, renderer='renderer', virtiofsd='virtiofsd')
            (root/'runtime.json').write_text(json.dumps(manifest))
            with patch('verify_portable_host.subprocess.run') as run:
                with self.assertRaisesRegex(ValueError, 'escapes'):
                    verify_portable_host.verify(root, root/'report.json')
                run.assert_not_called()

    def test_source_metadata_cannot_escape_the_cache(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root/'config').mkdir()
            (root/'config/sources.json').write_text(json.dumps({
                '../escape.tar.xz': dict(url='https://example.org/archive', sha256='a'*64)
            }))
            with patch.object(build_runtime, 'ROOT', root):
                with self.assertRaisesRegex(ValueError, 'invalid source archive'):
                    build_runtime.sources()

    def test_interrupted_extraction_does_not_leave_a_completed_source_tree(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root/'config').mkdir()
            output = root/'output'
            (output/'cache').mkdir(parents=True)
            archive = output/'cache/component.tar.xz'
            archive.write_bytes(b'source archive')
            (root/'config/sources.json').write_text(json.dumps({archive.name: dict(
                url='https://example.org/component.tar.xz',
                sha256=hashlib.sha256(archive.read_bytes()).hexdigest()
            )}))
            def interrupt(command):
                (Path(command[-1])/'partial').write_text('incomplete')
                raise RuntimeError('interrupted')
            with patch.object(build_runtime, 'ROOT', root), \
                    patch.object(build_runtime, 'OUT', output), \
                    patch.object(build_runtime, 'run', side_effect=interrupt):
                with self.assertRaisesRegex(RuntimeError, 'interrupted'):
                    build_runtime.sources()
            self.assertEqual(list((output/'sources').iterdir()), [])


if __name__ == '__main__':
    unittest.main()
