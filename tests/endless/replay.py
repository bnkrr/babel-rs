#!/usr/bin/env python3
"""Fresh-state replay of one seeded mutation round, not historical process state.

Pass --replay-round N followed by normal netns.py arguments (including daemon,
seed, topology, mix and --artifacts). Uses the same oracle and cleanup contracts.
"""
import argparse
import hashlib
from pathlib import Path
import signal
import sys
import tempfile
import time

from netns import Runner, StopRequested, arguments


def main():
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument("--replay-round", type=int, required=True)
    selected, remaining = parser.parse_known_args()
    if not 1 <= selected.replay_round <= 100_000:
        parser.error("replay round must be in 1..100000")
    sys.argv = [sys.argv[0], *remaining]
    args, topology = arguments()
    if args.artifacts is None:
        parser.error("--artifacts is required")
    args.artifacts.mkdir(parents=True, exist_ok=False)
    for _ in range((selected.replay_round - 1) * args.changes):
        topology.next_event()
    with tempfile.TemporaryDirectory(prefix="babel-replay-") as directory:
        runner = Runner(args, topology, Path(directory), args.artifacts)
        runner.round = selected.replay_round - 1
        runner.save("manifest.json", {"fixture": "fresh-state single-round replay",
                    "seed": args.seed, "edges": topology.edges, "initial_topology": topology.state(),
                    "round": selected.replay_round, "mix": args.mix,
                    "implementations": runner.implementations,
                    "namespace_prefix": f"vbe-{runner.token}-",
                    "binary_sha256": hashlib.sha256(args.daemon.read_bytes()).hexdigest()})

        def stop(*_):
            raise StopRequested("operator stop")

        for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
            signal.signal(sig, stop)
        code = 0
        try:
            runner.deadline = time.monotonic() + args.settle_timeout
            for node in sorted(topology.active):
                runner.add_node(node)
            runner.verify([])
            runner.round = selected.replay_round
            start = time.monotonic()
            runner.deadline = start + args.settle_timeout
            for _ in range(args.changes):
                runner.apply(topology.next_event())
            runner.verify([])
            runner.save("result.json", {"result": "PASS", "seconds": time.monotonic() - start,
                                       "verification": runner.verification,
                                       "scope": "fresh-state topology/event replay, not prior sequence history"})
        except StopRequested as error:
            code = 130
            runner.snapshot_failure(error)
        except Exception as error:
            code = 1
            runner.snapshot_failure(error)
        finally:
            for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
                signal.signal(sig, signal.SIG_IGN)
            cleanup_errors = runner.cleanup()
            if cleanup_errors:
                code = 1
            runner.save("outcome.json", {"exit_code": code, "cleanup_ok": not cleanup_errors})
        return code


if __name__ == "__main__":
    raise SystemExit(main())
