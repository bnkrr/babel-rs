"""Manual verifier diagnostics regression; no network or root needed."""
import copy
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch

from netns import Runner
from model import Topology
from test_model import observations


class VerifierTests(unittest.TestCase):
    def make_runner(self, mutate):
        runner = Runner.__new__(Runner)
        runner.args = SimpleNamespace(settle_timeout=30, stable_seconds=10)
        runner.topology = Topology(3, 2, "mesh", 1)
        runner.nodes = {node: {} for node in range(3)}
        runner.implementations = {node: "babel-rs" for node in range(3)}
        runner.round, runner.started = 1, 0
        runner.counts = {}
        sample = observations(runner.topology)
        for observation in sample.values():
            observation['status'] = {'route_generation': 5, 'export': {
                'last_error': None, 'last_success_route_generation': 5,
                'last_success_config_generation': 0, 'config_generation': 0,
                'last_success_age_seconds': 0}}
        clock = [0]

        def observe():
            result = copy.deepcopy(sample)
            mutate(result, clock[0])
            runner.latest = result
            return result

        runner.observe = observe
        runner.probes = Mock()
        runner.record, runner.save = Mock(), Mock()
        return runner, clock

    def verify(self, runner, clock):
        with patch('netns.time.monotonic', side_effect=lambda: clock[0]), \
                patch('netns.time.sleep', side_effect=lambda seconds: clock.__setitem__(0, clock[0] + seconds)):
            runner.verify([])

    def test_moving_generation_with_bounded_lag_passes(self):
        def mutate(sample, now):
            for observation in sample.values():
                observation['status']['route_generation'] = 5 + int(now)
                observation['status']['export']['last_success_route_generation'] = 4 + int(now)
        runner, clock = self.make_runner(mutate)
        self.verify(runner, clock)
        self.assertEqual(clock[0], 10)
        self.assertEqual(runner.verification['resets'], 0)
        self.assertTrue(all(c['route'] == 5 for c in runner.verification['export_checkpoints'].values()))

    def test_frozen_ack_cannot_pass_even_with_fresh_success_age(self):
        def mutate(sample, now):
            sample[1]['status']['route_generation'] = 5 + int(now)
            sample[1]['status']['export']['last_success_route_generation'] = 4
        runner, clock = self.make_runner(mutate)
        with self.assertRaisesRegex(TimeoutError, 'target_route='):
            self.verify(runner, clock)
        self.assertFalse(any(c.args[0] == 'verified' for c in runner.record.call_args_list))
        self.assertGreater(runner.verification['resets'], 0)

    def test_export_error_resets_window_and_retains_evidence(self):
        def mutate(sample, now):
            if now == 2:
                sample[1]['status']['export']['last_error'] = 'injected netlink failure'
        runner, clock = self.make_runner(mutate)
        self.verify(runner, clock)
        self.assertEqual(clock[0], 14)
        self.assertEqual(runner.verification['resets'], 1)
        rejection = runner.verification['last_rejection']
        self.assertEqual(rejection['stage'], 'export')
        self.assertEqual(rejection['stable_seconds'], 2)
        self.assertIn('injected netlink failure', rejection['reason'])
        saved = {call.args[0]: call.args[1] for call in runner.save.call_args_list}
        self.assertIn('latest.json', saved)
        self.assertEqual(saved['last-reset.json']['observations'][1]['status']['export']['last_error'], 'injected netlink failure')

    def test_forwarding_failure_still_resets_global_window(self):
        runner, clock = self.make_runner(lambda sample, now: None)
        from model import NotConverged
        def probe(_pairs):
            if clock[0] == 2:
                raise NotConverged('injected forwarding loss')
        runner.probes = probe
        self.verify(runner, clock)
        self.assertEqual(clock[0], 14)
        self.assertEqual(runner.verification['last_rejection']['stage'], 'probes')

    def test_stale_export_health_never_passes(self):
        def mutate(sample, now):
            sample[1]['status']['export']['last_success_age_seconds'] = 10
        runner, clock = self.make_runner(mutate)
        with self.assertRaises(TimeoutError):
            self.verify(runner, clock)
        self.assertFalse(any(c.args[0] == 'verified' for c in runner.record.call_args_list))

    def test_mixed_nodes_keep_babel_rs_export_checks(self):
        def mutate(sample, now):
            for node, kind in ((1, 'bird'), (2, 'babeld')):
                sample[node].update(implementation=kind, status='foreign control reply', routes=None)
        runner, clock = self.make_runner(mutate)
        runner.implementations.update({1: 'bird', 2: 'babeld'})
        self.verify(runner, clock)
        self.assertEqual(set(runner.verification['export_checkpoints']), {0})
        self.assertEqual(clock[0], 10)

        def frozen(sample, now):
            mutate(sample, now)
            sample[0]['status']['export']['last_success_route_generation'] = 4
        runner, clock = self.make_runner(frozen)
        runner.implementations.update({1: 'bird', 2: 'babeld'})
        with self.assertRaisesRegex(TimeoutError, 'target_route='):
            self.verify(runner, clock)


if __name__ == '__main__':
    unittest.main()
