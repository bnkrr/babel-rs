#!/usr/bin/env python3
"""Exercise the public runtime policy API over real UDP sockets and a peer FIB."""
import importlib.util
from pathlib import Path
import select
import subprocess
import sys
import time

spec = importlib.util.spec_from_file_location("rfc_lab", Path(__file__).with_name("netns-rfc-boundaries.py"))
rfc = importlib.util.module_from_spec(spec)
spec.loader.exec_module(rfc)


def main(daemon, example):
    with rfc.Lab(daemon, "policy", 2) as lab:
        lab.link(0, 1)
        lab.config(1)
        lab.start(1)
        error_log = open(lab.root / "policy.log", "w")
        lab.logs.append(error_log)
        proc = subprocess.Popen([
            "ip", "netns", "exec", lab.spaces[0], example, "lan", "0101010101010101", "198.51.100.1/32",
        ], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=error_log, text=True, bufsize=1)
        lab.processes.append(proc)

        def read():
            assert select.select([proc.stdout], [], [], 5)[0], "policy command timed out"
            line = proc.stdout.readline().strip()
            assert line, f"policy example exited: {proc.poll()}"
            return line

        def command(value):
            proc.stdin.write(value + "\n")
            proc.stdin.flush()
            return read()

        def learned():
            status = command("status")
            assert status.startswith("neighbors=1 "), status
            return "198.51.100.2/32" in status

        assert read() == "ready"
        lab.wait("policy-initial-bidirectional", lambda: learned() and lab.routes(1, 0))
        assert command("deny-import") == "ok"
        assert not learned(), "command acknowledgement must follow local RIB reselection"
        assert lab.routes(1, 0), "import filtering must preserve local origin export"
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            assert not learned(), "periodic imports bypassed policy"
            time.sleep(0.2)
        assert command("allow") == "ok"
        lab.wait("policy-import-relearns", learned)
        assert command("deny-export") == "ok"
        assert learned(), "export filtering must preserve the local RIB"
        lab.wait("policy-export-retracts-peer-fib", lambda: not lab.routes(1, 0), seconds=5)
        # Cover periodic output and repeated withdrawals after the transition.
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            assert not lab.routes(1, 0), "a queued/repeated/periodic finite Update bypassed policy"
            time.sleep(0.2)
        assert command("allow") == "ok"
        lab.wait("policy-export-recovers-peer-fib", lambda: lab.routes(1, 0), seconds=5)
        proc.stdin.write("quit\n")
        proc.stdin.flush()
        assert proc.wait(timeout=6) == 0
        proc.stdin.close()
        proc.stdout.close()
        print("route-policy E2E passed", flush=True)


if __name__ == "__main__":
    if len(sys.argv) != 3:
        raise SystemExit("usage: netns-route-policy.py BABEL_RS ROUTE_POLICY_EXAMPLE")
    main(*(str(Path(arg).resolve()) for arg in sys.argv[1:]))
