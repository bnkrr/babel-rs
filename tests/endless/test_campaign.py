"""Manual supervisor checks, using disposable unprivileged child processes."""
import json
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from campaign import next_action, run_campaign


class CampaignTests(unittest.TestCase):
    def test_cleanup_and_startup_failure_guards(self):
        self.assertEqual(next_action(1, {}, 0)[0], 'cleanup-failed')
        self.assertEqual(next_action(-9, {'cleanup_ok': True}, 0)[0], 'child-aborted')
        self.assertEqual(next_action(130, {'cleanup_ok': True}, 0)[0], 'stopped')
        failures = 0
        for _ in range(3):
            action, failures = next_action(1, {'cleanup_ok': True, 'initial_verified': False}, failures)
        self.assertEqual((action, failures), ('startup-failures', 3))
        self.assertEqual(next_action(1, {'cleanup_ok': True, 'initial_verified': True}, 2), ('next-seed', 0))

    def test_failed_runs_are_retained_with_new_seed_and_clean_directory(self):
        script = '''import json,sys
from pathlib import Path
p=Path(sys.argv[1]);p.mkdir(exist_ok=False)
seed=int(sys.argv[2]);code=1 if seed<3 else 0
(p/'outcome.json').write_text(json.dumps(dict(exit_code=code,cleanup_ok=True,initial_verified=True)))
if code: (p/'failure.json').write_text(json.dumps(dict(seed=seed)))
sys.exit(code)
'''
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / 'campaign'
            args = SimpleNamespace(seed=1, nodes=3)
            def command(_args, seed, artifacts):
                return [sys.executable, '-c', script, str(artifacts), str(seed)]
            with patch('campaign.child_command', side_effect=command), \
                    patch('campaign.shutil.disk_usage', return_value=SimpleNamespace(free=2**40)):
                self.assertEqual(run_campaign(args, root), 1)  # Later pass does not erase failures.
            summary = json.loads((root/'campaign.json').read_text())
            self.assertEqual(summary['failed_runs'], 2)
            self.assertEqual(summary['attempts'], 3)
            for seed in (1, 2):
                self.assertEqual(json.loads((root/f'run-{seed:06d}-seed-{seed}'/'failure.json').read_text())['seed'], seed)


if __name__ == '__main__':
    unittest.main()
