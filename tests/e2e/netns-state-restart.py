#!/usr/bin/env python3
"""Root-only A -> B -> C checkpoint and restart recovery test.

Uses production Hello/Update defaults (4 s / 16 s). Each unclean restart has a
240-second recovery deadline: a random sequence number may be behind surviving
feasibility history. Requires only Python 3, iproute2, sysctl and ping.
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


ROUTER_A = "1111111111111111"
PREFIX_A = "2001:db8:a::1/128"
PREFIX_C = "2001:db8:c::1/128"
RECOVERY_TIMEOUT = 240


def run(*args):
    result = subprocess.run(args, text=True, capture_output=True, timeout=10)
    if result.returncode:
        raise RuntimeError(f"{args}: {result.stdout}\n{result.stderr}")
    return result.stdout


def read_state(path):
    # The daemon emits a flat TOML document containing strings and integers;
    # avoid requiring Python 3.11's tomllib on the test VM.
    result = {}
    for line in path.read_text().splitlines():
        if "=" in line:
            key, value = line.split("=", 1)
            result[key.strip()] = json.loads(value.strip())
    return result


def main():
    def interrupted(_signum, _frame):
        raise KeyboardInterrupt("test interrupted")

    signal.signal(signal.SIGTERM, interrupted)
    daemon = str(Path(sys.argv[1]).resolve())
    assert os.geteuid() == 0, "run on the designated root test VM"
    assert os.access(daemon, os.X_OK), f"not executable: {daemon}"
    namespaces = {node: f"vb-state-{node}-{os.getpid()}" for node in "abc"}
    processes = {}
    created = []
    links = []
    logs = []
    results = []
    with tempfile.TemporaryDirectory(prefix="babel-rs-state-restart.") as directory:
        runtime = Path(directory)
        state_path = runtime / "a.state"

        def command(node, cmd, **params):
            with socket.socket(socket.AF_UNIX) as sock:
                sock.settimeout(3)
                sock.connect(str(runtime / f"{node}.ctl"))
                with sock.makefile("rwb") as stream:
                    json.loads(stream.readline())
                    request = {"api_version": 1, "id": 1, "command": cmd, "params": params}
                    stream.write((json.dumps(request) + "\n").encode())
                    stream.flush()
                    response = json.loads(stream.readline())
                    assert response["ok"], response
                    return response["result"]

        def wait_for(predicate, description, timeout=60):
            started = time.monotonic()
            while time.monotonic() - started < timeout:
                assert all(proc.poll() is None for proc in processes.values()), "daemon exited"
                try:
                    result = predicate()
                    if result:
                        return result
                except (FileNotFoundError, ConnectionRefusedError):
                    pass
                time.sleep(0.2)
            raise AssertionError(f"timed out after {timeout}s: {description}")

        def config(node, metric=100, marker=None, origins=True):
            router_id = {"a": ROUTER_A, "b": "2222222222222222", "c": "3333333333333333"}[node]
            text = f'''router_id = "{router_id}"
state_file = "{runtime}/{node}.state"
[[interfaces]]
match = ["babel*"]
[interfaces.metric]
type = "wired"
'''
            prefixes = []
            if node == "a" and origins:
                prefixes.append(PREFIX_A)
                if marker is not None:
                    prefixes.append(f"2001:db8:a::{marker}/128")
            elif node == "c":
                prefixes.append(PREFIX_C)
            for prefix in prefixes:
                text += f'[[origins]]\ndestination = "{prefix}"\nmetric = {metric}\n'
            text += '''[export]
protocol = 203
manage_rules = false
[[export.views]]
table = 25101
'''
            (runtime / f"{node}.toml").write_text(text)

        def start(node):
            # A stale pathname must never be mistaken for the new control socket.
            (runtime / f"{node}.ctl").unlink(missing_ok=True)
            log = open(runtime / f"{node}-{len(logs)}.log", "w")
            logs.append(log)
            processes[node] = subprocess.Popen(
                ["ip", "netns", "exec", namespaces[node], daemon, "run", "--config",
                 str(runtime / f"{node}.toml"), "--control-socket", str(runtime / f"{node}.ctl")],
                stdout=log, stderr=log)
            return wait_for(lambda: command(node, "status"), f"{node} control readiness", 15)

        def stop(node, graceful=True):
            proc = processes[node]
            proc.send_signal(signal.SIGTERM if graceful else signal.SIGKILL)
            status = proc.wait(timeout=8)
            del processes[node]
            assert status == (0 if graceful else -signal.SIGKILL), (node, status)

        def sequence():
            return command("a", "status")["sequence_number"]

        def routes(node, prefix):
            return command(node, "routes", destination=prefix)["routes"]

        def running_state():
            state = read_state(state_path)
            assert state["version"] == 2, state
            assert state["router_id"].replace(":", "").lower() == ROUTER_A, state
            assert "sequence_number" not in state, state
            return state

        def ping():
            def once():
                # The route snapshot can precede asynchronous kernel export.
                try:
                    run("ip", "netns", "exec", namespaces["c"], "ping", "-6", "-c", "1", "-W", "1",
                        "-I", "2001:db8:c::1", "2001:db8:a::1")
                    return True
                except RuntimeError:
                    return False
            wait_for(once, "two-hop forwarding", 8)

        def recovered(marker):
            seqno = sequence()
            selected = routes("c", PREFIX_A)
            # Kernel routes/ping alone can pass with a stale pre-crash route.
            # Require this instance's sequence number and its newly added origin.
            if not (selected and selected[0]["sequence_number"] == seqno
                    and selected[0]["router_id"].replace(":", "").lower() == ROUTER_A):
                return False
            if marker is not None and not routes("c", f"2001:db8:a::{marker}/128"):
                return False
            return bool(routes("a", PREFIX_C))

        def record_restart(kind, old_seqno, marker, expected=None):
            started = time.monotonic()
            initial = start("a")["sequence_number"]
            running_state()
            if expected is not None:
                # Readiness precedes adjacency re-establishment. Permit a few
                # legitimate increments if a fast Seqno Request raced the read.
                assert ((initial - expected) & 65535) < 16, (initial, expected)
            wait_for(lambda: recovered(marker), f"{kind}: new instance propagated to C", RECOVERY_TIMEOUT)
            ping()
            result = {"scenario": kind, "previous_sequence_number": old_seqno,
                      "startup_sequence_number": initial, "recovered_sequence_number": sequence(),
                      "startup_behind_peer": 0 < ((old_seqno - initial) & 65535) < 32768,
                      "recovery_seconds": round(time.monotonic() - started, 3)}
            results.append(result)
            print(json.dumps(result), flush=True)

        try:
            for node, ns in namespaces.items():
                run("ip", "netns", "add", ns)
                created.append(ns)
                run("ip", "-n", ns, "link", "set", "lo", "up")
                run("ip", "netns", "exec", ns, "sysctl", "-qw", "net.ipv6.conf.all.forwarding=1")
                run("ip", "-n", ns, "-6", "rule", "add", "priority", "1000", "lookup", "25101")
                if node in "ac":
                    run("ip", "-n", ns, "-6", "addr", "add", f"2001:db8:{node}::1/128", "dev", "lo")
            for index, (left, right) in enumerate([("a", "b"), ("b", "c")]):
                dev_left, dev_right = f"vsl{index}{os.getpid()}", f"vsr{index}{os.getpid()}"
                run("ip", "link", "add", dev_left, "type", "veth", "peer", "name", dev_right)
                links.extend([dev_left, dev_right])
                for device, node, address in [(dev_left, left, f"fe80::{index + 1}1"),
                                               (dev_right, right, f"fe80::{index + 1}2")]:
                    interface = f"babel{index}"
                    ns = namespaces[node]
                    run("ip", "link", "set", device, "netns", ns, "name", interface)
                    run("ip", "-n", ns, "link", "set", interface, "addrgenmode", "none")
                    run("ip", "-n", ns, "-6", "addr", "add", f"{address}/64", "dev", interface, "nodad")
                    run("ip", "-n", ns, "link", "set", interface, "up")
            for node in "abc":
                config(node)
                start(node)
            wait_for(lambda: recovered(None), "initial two-hop convergence")
            ping()
            running_state()
            initial_bytes = state_path.read_bytes()
            initial_inode = state_path.stat().st_ino

            # Real origin changes advance seqno, while the consumed checkpoint
            # stays byte-for-byte and inode-for-inode unchanged throughout run.
            for metric in [110, 120]:
                before = sequence()
                config("a", metric=metric)
                command("a", "reload")
                wait_for(lambda: sequence() != before and recovered(None), "origin metric sequence update")
                assert state_path.read_bytes() == initial_bytes
                assert state_path.stat().st_ino == initial_inode
            config("a", origins=False)
            before = sequence()
            command("a", "reload")
            wait_for(lambda: sequence() != before and not routes("c", PREFIX_A), "origin withdrawal")
            final_seqno = sequence()
            assert state_path.read_bytes() == initial_bytes
            assert state_path.stat().st_ino == initial_inode
            stop("a")
            checkpoint = read_state(state_path)
            assert checkpoint["sequence_number"] == final_seqno, (checkpoint, final_seqno)
            config("a", marker="10")
            record_restart("graceful", final_seqno, "10", (final_seqno + 1) & 65535)

            previous = routes("c", PREFIX_A)[0]["sequence_number"]
            stop("a", graceful=False)
            running_state()
            config("a", marker="20")
            record_restart("sigkill-random-sequence", previous, "20")

            previous = routes("c", PREFIX_A)[0]["sequence_number"]
            stop("a", graceful=False)
            state_path.unlink()
            config("a", marker="30")
            record_restart("missing-state-explicit-router-id", previous, "30")

            # A deterministic backwards start supplements the random crash
            # cases, which otherwise have a 50% chance of avoiding old FD.
            # This also verifies migration of the former v1 state file format.
            previous = routes("c", PREFIX_A)[0]["sequence_number"]
            stop("a", graceful=False)
            saved = (previous - 8193) & 65535
            state_path.write_text(f'version = 1\nrouter_id = "{ROUTER_A}"\nsequence_number = {saved}\n')
            config("a", marker="40")
            record_restart("stale-v1-checkpoint-behind-peer", previous, "40", (saved + 1) & 65535)
            assert results[-1]["startup_behind_peer"], results[-1]
            print(json.dumps({"test": "state-restart", "result": "PASS", "scenarios": results}), flush=True)
        except Exception:
            for node in processes:
                for cmd, params in [("status", {}), ("neighbors", {}), ("routes", {"destination": PREFIX_A})]:
                    try:
                        print(node, cmd, command(node, cmd, **params), file=sys.stderr)
                    except Exception as error:
                        print(node, cmd, error, file=sys.stderr)
            raise
        finally:
            for proc in processes.values():
                if proc.poll() is None:
                    proc.terminate()
            for proc in processes.values():
                try:
                    proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait(timeout=5)
            for log in logs:
                log.close()
            if sys.exc_info()[0] is not None:
                for log in runtime.glob("*.log"):
                    print(log, log.read_text()[-6000:], file=sys.stderr)
            for ns in reversed(created):
                subprocess.run(["ip", "netns", "del", ns], check=False, capture_output=True, timeout=10)
            # These remain in the root namespace only if setup failed mid-link.
            for link in links:
                subprocess.run(["ip", "link", "del", link], check=False, capture_output=True, timeout=10)


if __name__ == "__main__":
    main()
