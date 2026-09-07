#!/usr/bin/env python3
"""Root-only shutdown deadline / cross-table startup cleanup regression.

Pass the locally built daemon and netlink-stall.so. The preload only affects
the test daemon, not iproute2 or other processes. No production fault hooks.
"""
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import time


def run(*args):
    result = subprocess.run(args, capture_output=True, text=True, timeout=10)
    if result.returncode:
        raise RuntimeError(f"{args}: {result.stdout}\n{result.stderr}")
    return result.stdout


def main():
    def interrupted(_signum, _frame):
        raise KeyboardInterrupt("test interrupted")

    signal.signal(signal.SIGTERM, interrupted)
    daemon, shim = (str(Path(value).resolve()) for value in sys.argv[1:])
    assert os.geteuid() == 0, "run on the designated root test VM"
    assert os.access(daemon, os.X_OK) and Path(shim).is_file()
    namespace = f"vb-exit-{os.getpid()}"
    foreign_namespace = f"vb-other-{os.getpid()}"
    namespaces = []
    processes = []
    results = []
    with tempfile.TemporaryDirectory(prefix="babel-shutdown-recovery.") as directory:
        root = Path(directory)
        config = root / "daemon.toml"
        control = root / "daemon.ctl"
        state = root / "daemon.state"
        marker = root / "stall"
        netlink_hit = root / "netlink.hit"
        fsync_hit = root / "fsync.hit"

        def write_config(timeout_ms=None, manage_rules=True, table=23101):
            text = f'router_id = "1234567812345678"\nstate_file = "{state}"\n'
            if timeout_ms is not None:
                text += f'shutdown_timeout_ms = {timeout_ms}\n'
            text += f'''[[interfaces]]
match = ["absent*"]
[[origins]]
destination = "2001:db8:1234::/64"
[export]
protocol = 205
manage_rules = {str(manage_rules).lower()}
[[export.views]]
table = {table}
source = "10.11.0.0/16"
[[export.views]]
table = {table + 1}
source = "2001:db8:11::/48"
'''
            config.write_text(text)

        def command(name, expected_ok=True):
            with socket.socket(socket.AF_UNIX) as sock:
                sock.settimeout(3)
                sock.connect(str(control))
                with sock.makefile("rwb") as stream:
                    json.loads(stream.readline())
                    stream.write((json.dumps({"api_version": 1, "id": 1,
                                              "command": name, "params": {}}) + "\n").encode())
                    stream.flush()
                    reply = json.loads(stream.readline())
                    assert reply["ok"] == expected_ok, reply
                    return reply.get("result")

        def wait_for(predicate, description, timeout=10):
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                try:
                    result = predicate()
                    if result:
                        return result
                except (FileNotFoundError, ConnectionRefusedError):
                    pass
                time.sleep(0.05)
            raise AssertionError(f"timed out: {description}")

        def start(label, stall_fsync=False):
            environment = os.environ.copy()
            environment.update(LD_PRELOAD=shim, RUST_LOG="info",
                               BABEL_TEST_STALL_MARKER=str(marker),
                               BABEL_TEST_NETLINK_HIT=str(netlink_hit),
                               BABEL_TEST_FSYNC_HIT=str(fsync_hit))
            if stall_fsync:
                environment["BABEL_TEST_STALL_FSYNC"] = "1"
            logfile = root / f"{len(processes)}-{label}.log"
            with logfile.open("w") as log:
                proc = subprocess.Popen(
                    ["ip", "netns", "exec", namespace, daemon, "run", "--config", str(config),
                     "--control-socket", str(control)], env=environment, stdout=log, stderr=log)
            processes.append(proc)
            return proc, logfile

        def ready(proc):
            def check():
                assert proc.poll() is None, f"daemon exited: {proc.returncode}"
                status = command("status")
                return status if status["export"]["last_success_age_seconds"] is not None else None
            return wait_for(check, "daemon and initial export ready")

        def owned(ns=namespace, protocol=205):
            routes, rules = [], []
            for family in ("-4", "-6"):
                routes += run("ip", "-n", ns, family, "route", "show", "table", "all",
                              "proto", str(protocol)).splitlines()
                entries = json.loads(run("ip", "-n", ns, family, "-j", "-details", "rule", "show"))
                rules += [entry for entry in entries if str(entry.get("protocol")) == str(protocol)]
            return routes, rules

        def seed_stale():
            # Include a table absent from both old and new config, default
            # routes, an unreachable tombstone, and an unmodelled 'from all'
            # rule. All are within this daemon's protocol ownership scope.
            for family in ("-4", "-6"):
                run("ip", "-n", namespace, family, "route", "add", "blackhole", "default",
                    "table", "23899", "proto", "205")
                run("ip", "-n", namespace, family, "rule", "add", "pref", "23899",
                    "from", "all", "lookup", "23899", "protocol", "205")
            run("ip", "-n", namespace, "-6", "route", "add", "unreachable", "2001:db8:dead::/64",
                "table", "23900", "proto", "205")

        def normal_stop(proc):
            started = time.monotonic()
            command("shutdown")
            assert proc.wait(timeout=6) == 0
            assert owned() == ([], []), owned()
            return round(time.monotonic() - started, 3)

        def restart_and_check():
            marker.unlink(missing_ok=True)
            write_config(manage_rules=False, table=24101)
            proc, _ = start("recovery")
            ready(proc)
            wait_for(lambda: owned() == ([], []), "old routes and rules cleaned across tables")
            assert owned(protocol=206) == foreign_protocol
            assert owned(ns=foreign_namespace) == foreign_netns
            normal_stop(proc)

        try:
            for ns in (namespace, foreign_namespace):
                run("ip", "netns", "add", ns)
                namespaces.append(ns)
                run("ip", "-n", ns, "link", "set", "lo", "up")
            for ns, protocol in ((namespace, 206), (foreign_namespace, 205)):
                for family, source in (("-4", "192.0.2.0/24"), ("-6", "2001:db8:99::/48")):
                    run("ip", "-n", ns, family, "route", "add", "blackhole", "default",
                        "table", "23999", "proto", str(protocol))
                    run("ip", "-n", ns, family, "rule", "add", "pref", "23999", "from", source,
                        "lookup", "23999", "protocol", str(protocol))
            foreign_protocol = owned(protocol=206)
            foreign_netns = owned(ns=foreign_namespace)
            assert all(foreign_protocol) and all(foreign_netns)

            # Existing synchronous cleanup still completes within the default.
            write_config()
            proc, _ = start("orderly")
            assert ready(proc)["shutdown_timeout_ms"] == 5000
            assert len(owned()[1]) == 2
            seed_stale()
            results.append({"scenario": "orderly", "seconds": normal_stop(proc)})

            for label, timeout_ms, use_control, block_fsync in (
                ("default-sigterm-netlink-and-fsync-stall", 5000, False, True),
                ("reloaded-control-shutdown-netlink-stall", 700, True, False),
            ):
                write_config()
                proc, log = start(label, stall_fsync=block_fsync)
                assert ready(proc)["shutdown_timeout_ms"] == 5000
                if use_control:
                    write_config(timeout_ms=timeout_ms)
                    command("reload")
                    assert command("status")["shutdown_timeout_ms"] == timeout_ms
                    write_config(timeout_ms=0)
                    command("reload", expected_ok=False)
                    assert command("status")["shutdown_timeout_ms"] == timeout_ms
                    write_config(timeout_ms=timeout_ms)
                netlink_hit.unlink(missing_ok=True)
                marker.touch()
                wait_for(netlink_hit.exists, "actual in-flight route dump stalled")
                seed_stale()
                leftovers = owned()
                assert all(leftovers)
                # Status remains responsive while export reconciliation waits.
                assert command("status")["ready"]
                started = time.monotonic()
                if use_control:
                    command("shutdown")
                else:
                    proc.send_signal(signal.SIGTERM)
                assert proc.wait(timeout=timeout_ms / 1000 + 2) != 0
                elapsed = time.monotonic() - started
                assert timeout_ms / 1000 - 0.08 <= elapsed < timeout_ms / 1000 + 0.8, elapsed
                assert "shutdown deadline exceeded" in log.read_text()
                assert "router cleanup" in log.read_text()
                assert owned() == leftovers, "unexpected removal with netlink unavailable"
                if block_fsync:
                    assert fsync_hit.exists(), "checkpoint did not consume part of the total budget"
                    assert "sequence_number" not in state.read_text()
                else:
                    assert "sequence_number" in state.read_text()
                restart_and_check()
                result = {"scenario": label, "timeout_ms": timeout_ms,
                          "seconds": round(elapsed, 3), "restart_cleanup": "PASS"}
                results.append(result)
                print(json.dumps(result), flush=True)

            # Critical service failure takes the same bounded cleanup path.
            write_config(timeout_ms=700)
            control.write_text("not a socket")
            netlink_hit.unlink(missing_ok=True)
            marker.touch()
            seed_stale()
            started = time.monotonic()
            proc, log = start("control-service-failure")
            assert proc.wait(timeout=3) != 0
            elapsed = time.monotonic() - started
            assert 0.6 <= elapsed < 2, elapsed
            assert netlink_hit.exists()
            assert "control server failed" in log.read_text()
            assert "shutdown deadline exceeded" in log.read_text()
            control.unlink()
            restart_and_check()
            results.append({"scenario": "critical-service-failure", "seconds": round(elapsed, 3)})
            print(json.dumps({"test": "shutdown-recovery", "result": "PASS", "scenarios": results}), flush=True)
        except BaseException:
            for logfile in sorted(root.glob("*.log")):
                print(f"{logfile.name}:\n{logfile.read_text()}", file=sys.stderr)
            raise
        finally:
            for proc in processes:
                if proc.poll() is None:
                    proc.kill()
                    proc.wait(timeout=5)
            for ns in reversed(namespaces):
                run("ip", "netns", "del", ns)


if __name__ == "__main__":
    main()
