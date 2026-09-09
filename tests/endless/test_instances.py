"""Manual mixed-instance model and local-control framing regressions."""
import copy
from pathlib import Path
import tempfile
import unittest
from unittest.mock import MagicMock, patch

from campaign import child_command
from instances import assign_instances, foreign_command, launch, parse_mix
from model import NotConverged, Topology, audit
from netns import arguments
from test_model import observations, set_route


class InstancesTests(unittest.TestCase):
    def test_recreation_removes_stale_runtime_paths_but_preserves_checkpoint(self):
        with tempfile.TemporaryDirectory() as directory:
            runtime = Path(directory)
            for kind in ("babel-rs", "bird", "babeld"):
                for suffix in ("pid", "ctl", "state"):
                    (runtime / f"0.{suffix}").write_text("old instance")
                argv = launch(kind, "/test/daemon", 0, runtime, [(0, 1), (1, 2), (0, 2)], 201, 209)
                self.assertEqual(argv[0], "/test/daemon")
                self.assertFalse((runtime / "0.pid").exists())
                self.assertFalse((runtime / "0.ctl").exists())
                self.assertEqual((runtime / "0.state").read_text(), "old instance")
            config = (runtime / "0.conf").read_text()
            self.assertIn("interface e0\n", config)
            self.assertIn("interface e2\n", config)
            self.assertNotIn("interface e1\n", config)

    def test_weights_are_validated(self):
        for bad in ("", "bird", "bird=nan", "bird=-1", "bird=0", "bird=1,bird=2",
                    "bogus=1", "bird=1.5", "bird=1000001"):
            with self.assertRaises(ValueError, msg=bad):
                parse_mix(bad)
        self.assertEqual(parse_mix("babeld=1,babel-rs=2,bird=0"),
                         {"babel-rs": 2, "bird": 0, "babeld": 1})

    def test_assignment_repeats_and_does_not_change_event_plan(self):
        mix = "babel-rs=2,bird=1,babeld=1"
        slots = assign_instances(32, 7, mix)
        self.assertEqual(slots, assign_instances(32, 7, "babeld=1,bird=1,babel-rs=2"))
        self.assertEqual(set(slots.values()), {"babel-rs", "bird", "babeld"})
        self.assertEqual(set(assign_instances(32, 7, "babel-rs=0,bird=1").values()), {"bird"})
        left, right = Topology(32, 4, "mesh", 7, 16), Topology(32, 4, "mesh", 7, 16)
        for _ in range(1000):
            assign_instances(32, 7, mix)
            self.assertEqual(left.next_event(), right.next_event())
        self.assertEqual(slots, assign_instances(32, 7, mix))

    def test_max_alias_and_campaign_keep_mix_and_tools(self):
        with patch("sys.argv", ["netns.py", "--validate", "--max-nodes", "16", "--min-nodes", "8",
                                "--mix", "babel-rs=2,bird=1,babeld=1", "--bird", "/opt/bird"]):
            args, _ = arguments()
        self.assertEqual((args.nodes, args.min_nodes, args.settle_timeout), (16, 8, 600))
        args.daemon = Path("/opt/babel-rs")
        command = child_command(args, 8, Path("/tmp/test-result"))
        for option, value in (("--mix", args.mix), ("--bird", "/opt/bird"), ("--nodes", "16"), ("--seed", "8")):
            self.assertEqual(command[command.index(option) + 1], value)

    def test_foreign_fib_still_requires_reachability_and_no_loops(self):
        topology = Topology(3, 2, "mesh", 1)
        sample = observations(topology)
        for node, kind in ((1, "bird"), (2, "babeld")):
            sample[node].update(implementation=kind, routes=None)
        self.assertEqual(len(audit(topology, sample)), 6)
        for fault in ("missing", "gateway", "loop", "down", "source"):
            observed = copy.deepcopy(sample)
            model = copy.deepcopy(topology)
            if fault == "missing":
                observed[1]["fib"].pop()
            elif fault == "gateway":
                observed[1]["fib"][0]["gateway"] = "fe80::ffff"
            elif fault == "source":
                observed[1]["fib"][0]["from"] = "2001:db8:ffff::/64"
            elif fault == "loop":
                # Change only FIB for foreign nodes; there is no fabricated RIB.
                changed = observations(model)
                set_route(changed, model, 1, 0, 2)
                set_route(changed, model, 2, 0, 1)
                for node in (1, 2):
                    observed[node]["fib"] = changed[node]["fib"]
            else:
                model.enabled.remove(model.edges.index((1, 2)))
            with self.assertRaises(NotConverged, msg=fault):
                audit(model, observed)
        sample[0]["routes"]["routes"].pop()
        with self.assertRaisesRegex(NotConverged, "selected RIB"):
            audit(topology, sample)

    def control(self, kind, lines):
        connection = MagicMock()
        stream = connection.__enter__.return_value.makefile.return_value.__enter__.return_value
        stream.readline.side_effect = lines
        with patch("instances.socket.socket", return_value=connection):
            return foreign_command(kind, Path("/test.ctl"), "dump" if kind == "babeld" else "show status", 1)

    def test_foreign_control_framing(self):
        self.assertEqual(self.control("babeld", [b"BABEL 1.0\n", b"ok\n", b"add route ...\n", b"ok\n"]),
                         "add route ...\nok")
        self.assertEqual(self.control("bird", [b"0001 BIRD ready.\n", b"1000-data\n", b"0000\n"]),
                         "1000-data\n0000")
        for kind, lines in (("bird", [b"0001 BIRD ready.\n", b"9001 failure\n"]),
                            ("babeld", [b"BABEL 1.0\n", b"ok\n", b"bad command\n"]),
                            ("bird", [b"0001 BIRD ready.\n", b""]),
                            ("babeld", [b"x" * (1024 * 1024 + 1)])):
            with self.assertRaises(RuntimeError):
                self.control(kind, lines)


if __name__ == "__main__":
    unittest.main()
