#!/usr/bin/env python3
"""Manual root-only fixture regression; never included in normal tests or CI."""
import os
from pathlib import Path
import tempfile

from netns import Runner, arguments


def main():
    args, topology = arguments()
    if os.geteuid() != 0:
        raise SystemExit("run on a disposable root Linux test host")
    if args.artifacts is None:
        raise SystemExit("--artifacts must name a new directory")
    args.artifacts.mkdir(parents=True, exist_ok=False)
    with tempfile.TemporaryDirectory(prefix="babel-lifecycle-") as directory:
        runner = Runner(args, topology, Path(directory), args.artifacts)
        try:
            for node in sorted(topology.active):
                runner.add_node(node)
            # Keep the removed namespace alive deterministically: this also
            # covers delayed kernel namespace destruction without a timing race.
            node = topology.edges[0][0]
            for iteration in range(20):
                with open(f"/run/netns/{runner.namespace(node)}", "rb"):
                    runner.delete_node(node)
                    runner.add_node(node)
                assert len(runner.links) == len(topology.edges)
                runner.record("recreated", iteration=iteration + 1, node=node)
            # Cleanup must accept a tracked endpoint that disappeared with its
            # peer, including a partial add_link failure before either move.
            left, right = f"tl{runner.token}", f"tr{runner.token}"
            runner.run("ip", "link", "add", left, "type", "veth", "peer", "name", right)
            runner.temporary_links.update((left, right))
            absent = f"ta{runner.token}"
            runner.temporary_links.add(absent)
        finally:
            if runner.cleanup():
                raise RuntimeError("lifecycle cleanup failed")
        print("PASS: 20 immediate node recreations with retained namespace references; partial-link cleanup")


if __name__ == "__main__":
    main()
