"""Verify exact dependency pins, safe promotion, and actionable failure issues."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch


SCRIPTS = Path(__file__).resolve().parent


def load(name):
    spec = importlib.util.spec_from_file_location(name, SCRIPTS / (name + '.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


pinning = load('pin-openswap')
reporting = load('report-compatibility')


class Pins(unittest.TestCase):
    manifest = '[dependencies]\nopenswap = { git = "' + pinning.UPSTREAM + '", rev = "' + 'a' * 40 + '" }\nlog = "0.4"\n'

    def test_update_changes_only_upstream_revision(self):
        updated = pinning.pin_manifest(self.manifest, 'b' * 40)
        self.assertEqual(updated, self.manifest.replace('a' * 40, 'b' * 40))
        self.assertEqual(pinning.revision(updated), 'b' * 40)

    def test_invalid_hashes_are_rejected_before_manifest_update(self):
        for sha in ('master', 'a' * 39, 'A' * 40, 'a' * 40 + '\n'):
            with self.subTest(sha=sha), self.assertRaises(ValueError):
                pinning.pin_manifest(self.manifest, sha)

    def test_unexpected_dependency_is_rejected(self):
        for manifest in (self.manifest.replace(pinning.UPSTREAM, 'https://example.com/other'),
                         self.manifest.replace('rev =', 'branch ='),
                         '[dependencies]\nopenswap = "0.2"\n'):
            with self.subTest(manifest=manifest), self.assertRaises(ValueError):
                pinning.pin_manifest(manifest, 'b' * 40)

    def test_metadata_must_match_repository_revision_and_commit(self):
        expected = f'git+{pinning.UPSTREAM}?rev={"b" * 40}#{"b" * 40}'
        self.assertEqual(pinning.verify_metadata({'packages': [{'name': 'openswap', 'source': expected}]},
                                                'b' * 40), expected)
        for source in (expected.replace('#' + 'b' * 40, '#' + 'a' * 40),
                       expected.replace(pinning.UPSTREAM, 'https://example.com/other'), None):
            with self.subTest(source=source), self.assertRaises(ValueError):
                pinning.verify_metadata({'packages': [{'name': 'openswap', 'source': source}]}, 'b' * 40)
        for packages in ([], [{'name': 'openswap', 'source': expected}] * 2):
            with self.subTest(packages=packages), self.assertRaises(ValueError):
                pinning.verify_metadata({'packages': packages}, 'b' * 40)

    def test_failed_cargo_update_does_not_attempt_metadata_verification(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'Cargo.toml').write_text(self.manifest)
            with patch.object(pinning.subprocess, 'run', side_effect=subprocess.CalledProcessError(1, 'cargo')):
                with patch.object(pinning.subprocess, 'check_output') as metadata:
                    with self.assertRaises(subprocess.CalledProcessError):
                        pinning.pin(root, 'b' * 40)
            metadata.assert_not_called()


class Promotion(unittest.TestCase):
    def promote(self, status='behind', unchanged=False, moved=False, push_error=False):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            scripts = root / '.github/scripts'
            scripts.mkdir(parents=True)
            (scripts / 'pin-openswap.py').write_text((SCRIPTS / 'pin-openswap.py').read_text())
            commands = root / 'commands.jsonl'
            summary = root / 'summary.md'
            fake_git = root / 'git'
            fake_git.write_text('''#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
args = sys.argv[1:]
with open(os.environ['COMMANDS'], 'a') as log:
    log.write(json.dumps(args) + '\\n')
if args[:1] == ['show']:
    print('openswap = { git = "https://github.com/citadel-foss/openswap", rev = "' + 'a' * 40 + '" }')
if args[:1] == ['diff']:
    sys.exit(0 if os.environ['UNCHANGED'] == '1' else 1)
if args[:1] == ['rev-parse']:
    print('d' * 40 if os.environ['MOVED'] == '1' else 'c' * 40)
if args[:1] == ['push'] and os.environ['PUSH_ERROR'] == '1':
    print('error: push rejected', file=sys.stderr)
    sys.exit(1)
''')
            fake_git.chmod(0o755)
            fake_gh = root / 'gh'
            fake_gh.write_text('#!/usr/bin/env bash\nprintf "%s\\n" "$COMPARE_STATUS"\n')
            fake_gh.chmod(0o755)
            env = dict(os.environ, PATH=str(root) + os.pathsep + os.environ['PATH'],
                       COMMANDS=str(commands), GITHUB_STEP_SUMMARY=str(summary), GITHUB_SHA='c' * 40,
                       OPENSWAP_SHA='b' * 40, COMPARE_STATUS=status, UNCHANGED=str(int(unchanged)),
                       MOVED=str(int(moved)), PUSH_ERROR=str(int(push_error)))
            result = subprocess.run(['bash', str(SCRIPTS / 'promote-openswap.sh')], cwd=root,
                                    env=env, text=True, capture_output=True)
            return result, [json.loads(line) for line in commands.read_text().splitlines()], (
                summary.read_text() if summary.exists() else '')

    def test_success_commits_only_both_tested_dependency_files_without_force_push(self):
        result, commands, summary = self.promote()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(['add', 'Cargo.toml', 'Cargo.lock'], commands)
        self.assertIn(['push', 'origin', 'HEAD:main'], commands)
        self.assertIn('b' * 40, summary)

    def test_delayed_dispatch_does_not_roll_back_a_newer_pin(self):
        result, commands, summary = self.promote(status='ahead')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(any(command[0] in ('add', 'commit', 'push') for command in commands))
        self.assertIn('newer compatible', summary)

    def test_unchanged_pin_does_not_create_an_empty_commit(self):
        result, commands, _ = self.promote(unchanged=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(any(command[0] in ('commit', 'push') for command in commands))

    def test_changed_plugin_main_fails_before_commit(self):
        result, commands, _ = self.promote(moved=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('main changed during the build', result.stdout)
        self.assertFalse(any(command[0] in ('commit', 'push') for command in commands))

    def test_rejected_push_remains_a_failure_without_success_summary(self):
        result, _, summary = self.promote(push_error=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('push rejected', result.stderr)
        self.assertNotIn('Pinned OpenSwap', summary)

    def test_diverged_dependency_is_not_overwritten(self):
        result, commands, _ = self.promote(status='diverged')
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(any(command[0] in ('add', 'commit', 'push') for command in commands))


class Diagnostics(unittest.TestCase):
    def test_rust_error_keeps_source_location_and_removes_color_and_timestamps(self):
        summary, excerpt = reporting.error_excerpt(
            '2026-10-02T12:00:00Z Compiling dependencies\n'
            '2026-10-02T12:00:01Z \x1b[31merror[E0599]: no method named receive\x1b[0m\n'
            '2026-10-02T12:00:01Z   --> core/src/wallet.rs:42:5\n'
        )
        self.assertEqual(summary, 'error[E0599]: no method named receive')
        self.assertIn('core/src/wallet.rs:42:5', excerpt)
        self.assertNotIn('Compiling dependencies', excerpt)
        self.assertNotIn('\x1b', excerpt)
        self.assertNotIn('2026-', excerpt)

    def test_rejected_push_keeps_the_branch_protection_error(self):
        summary, excerpt = reporting.error_excerpt(
            'remote: error: GH006: Protected branch update failed for refs/heads/main.\n'
            'remote: Changes must be made through a pull request.\n'
            'error: failed to push some refs\n'
        )
        self.assertIn('GH006', summary)
        self.assertIn('Changes must be made through a pull request', excerpt)

    def test_unknown_failure_keeps_the_log_tail_and_escapes_markdown_fences(self):
        _, excerpt = reporting.error_excerpt('Preparing build\nSomething broke ```\n')
        self.assertIn('Something broke', excerpt)
        self.assertNotIn('```', excerpt)

    def test_generic_exit_status_keeps_preceding_unrecognized_error(self):
        summary, excerpt = reporting.error_excerpt(
            'Preparing installer\n'
            'failed to bundle application: missing resource\n'
            '##[error]Process completed with exit code 1.\n'
        )
        self.assertIn('see the job log', summary)
        self.assertIn('missing resource', excerpt)

    def run_report(self, existing):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            commands = []
            job = {'id': 5, 'conclusion': 'failure', 'name': 'Build native plugin',
                   'html_url': 'https://example.com/job/5',
                   'steps': [{'name': 'Build native plugin', 'conclusion': 'failure'}]}

            def fake_gh(*args):
                commands.append(args)
                endpoint = args[-1]
                if args[0] == 'api' and '/attempts/2/jobs?' in endpoint:
                    return json.dumps([{'jobs': [job]}])
                if args[0] == 'api' and endpoint.endswith('/jobs/5/logs'):
                    return 'error[E0599]: upstream API changed\n  --> core/src/ops.rs:8:2\n'
                if args[0] == 'api' and '/issues?' in endpoint:
                    return json.dumps([[{'number': 7, 'body': '<!-- openswap-compatibility:' + 'a' * 40 + ' -->',
                                         'html_url': 'https://example.com/issues/7'}]] if existing else [[]])
                if args[:2] == ('issue', 'create'):
                    return 'https://example.com/issues/8\n'
                return ''

            env = {'GITHUB_REPOSITORY': 'citadel-foss/btcpay-plugin', 'GITHUB_RUN_ID': '1',
                   'GITHUB_RUN_ATTEMPT': '2', 'OPENSWAP_SHA': 'a' * 40,
                   'PLUGIN_SHA': 'b' * 40, 'GITHUB_STEP_SUMMARY': str(root / 'summary.md')}
            previous = Path.cwd()
            try:
                os.chdir(root)
                with patch.dict(os.environ, env), patch.object(reporting, 'gh', side_effect=fake_gh):
                    reporting.report()
                body = (root / 'compatibility-report/issue.md').read_text()
                self.assertIn('Build native plugin', body)
                self.assertIn('Build native plugin', body)
                self.assertIn('error[E0599]', body)
                self.assertIn('core/src/ops.rs:8:2', body)
                self.assertTrue((root / 'compatibility-report/job-5.log').exists())
            finally:
                os.chdir(previous)
            return commands

    def test_failure_creates_issue_with_diagnostic_and_failed_platform(self):
        commands = self.run_report(existing=False)
        self.assertEqual(sum(command[:2] == ('issue', 'create') for command in commands), 1)
        self.assertFalse(any(command[:2] == ('issue', 'edit') for command in commands))

    def test_repeat_failure_updates_existing_issue_instead_of_creating_duplicate(self):
        commands = self.run_report(existing=True)
        self.assertTrue(any(command[:3] == ('issue', 'edit', '7') for command in commands))
        self.assertTrue(any(command[:3] == ('issue', 'comment', '7') for command in commands))
        self.assertFalse(any(command[:2] == ('issue', 'create') for command in commands))


if __name__ == '__main__':
    unittest.main()
