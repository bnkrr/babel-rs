#!/usr/bin/env python3
"""Root-only steady forwarding with repeated leaf failures and origin changes.

A -- B -- C is the healthy path; D is a leaf attached to B. Production Hello
and Update timers are used. The duration is measured after initial convergence.
Each cycle adds/withdraws a C origin, flaps D's link, and restarts D gracefully.
Every poll checks A <-> C forwarding, live processes, RIB and output budgets.
Run on a disposable host: python3 netns-steady-state.py ./babel-rs --seconds 3600
"""
import argparse
import json
import os
from pathlib import Path
import signal
import socket
import statistics
import subprocess
import tempfile
import time


TABLE = 25201
PROTOCOL = 207
NODES = "abcd"
ANCHOR = {node: f"2001:db8:{node}::1/128" for node in "acd"}
EXTRA = "2001:db8:c::2/128"


def run(*args):
    result = subprocess.run(args, text=True, capture_output=True, timeout=10)
    if result.returncode:
        raise RuntimeError(f"{args}: {result.stdout}\n{result.stderr}")
    return result.stdout


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("daemon", type=Path)
    parser.add_argument("--seconds", type=int, default=3600)
    parser.add_argument("--seed", type=int, default=1,
                        help="reproducible leaf metric choices, recorded in the report")
    parser.add_argument("--rss-growth-kib", type=int, default=16384,
                        help="allowed median RSS growth after warmup; detects gross growth, not all leaks")
    args = parser.parse_args()
    assert os.geteuid() == 0, "run on a disposable root test host"
    assert args.seconds >= 120, "at least 120 seconds is required to exercise all phases"
    assert args.rss_growth_kib >= 0
    daemon = str(args.daemon.resolve())
    assert os.access(daemon, os.X_OK), daemon

    def interrupted(_signum, _frame):
        raise KeyboardInterrupt("test interrupted")

    signal.signal(signal.SIGTERM, interrupted)
    namespaces = {node: f"vb-soak-{node}-{os.getpid()}" for node in NODES}
    processes, created, links, logs = {}, [], [], []
    samples, counts = [], {name: 0 for name in ["announce", "withdraw", "link-down", "link-up", "restart"]}
    leaf_attached = True
    leaf_seq = args.seed & 0xffff
    success = False
    with tempfile.TemporaryDirectory(prefix="babel-rs-steady-state.") as directory:
        runtime = Path(directory)

        def command(node, cmd, **params):
            with socket.socket(socket.AF_UNIX) as sock:
                sock.settimeout(3)
                sock.connect(str(runtime / f"{node}.ctl"))
                with sock.makefile("rwb") as stream:
                    json.loads(stream.readline())
                    stream.write((json.dumps({"api_version": 1, "id": 1, "command": cmd, "params": params}) + "\n").encode())
                    stream.flush()
                    reply = json.loads(stream.readline())
                    assert reply["ok"], reply
                    return reply["result"]

        def config(node, extra=False, metric=0):
            text = f'''router_id = "{':'.join([f'{ord(node):02x}'] * 8)}"
state_file = "{runtime}/{node}.state"
[limits]
max_neighbors = 8
max_candidates = 512
max_candidates_per_neighbor = 256
[[interfaces]]
match = ["babel*"]
'''
            if node in ANCHOR:
                text += f'[[origins]]\ndestination = "{ANCHOR[node]}"\nmetric = {metric}\n'
            if extra:
                text += f'[[origins]]\ndestination = "{EXTRA}"\n'
            text += f'''[export]
protocol = {PROTOCOL}
manage_rules = false
[[export.views]]
table = {TABLE}
'''
            (runtime / f"{node}.toml").write_text(text)

        def start(node):
            log = open(runtime / f"{node}-{len(logs)}.log", "w")
            logs.append(log)
            processes[node] = subprocess.Popen([
                "ip", "netns", "exec", namespaces[node], daemon, "run",
                "--config", str(runtime / f"{node}.toml"),
                "--control-socket", str(runtime / f"{node}.ctl"),
            ], stdout=log, stderr=log)

        def has_route(node, prefix):
            return bool(command(node, "routes", destination=prefix)["routes"])

        def restore_leaf_link():
            run("ip", "-n", namespaces["b"], "link", "set", "babel2", "up")
            # Linux removes link-local addresses on admin-down. With automatic
            # address generation disabled in this fixture, the host must restore
            # them before expecting Babel's interface supervisor to reattach.
            for node, interface, address in [("b", "babel2", "fe80::b2"), ("d", "babel0", "fe80::d")]:
                run("ip", "-n", namespaces[node], "-6", "addr", "replace", f"{address}/64",
                    "dev", interface, "nodad")

        def ping():
            for source, destination in [("a", "c"), ("c", "a")]:
                run("ip", "netns", "exec", namespaces[source], "ping", "-6", "-n",
                    "-c", "1", "-W", "2", "-I", ANCHOR[source].split("/")[0],
                    ANCHOR[destination].split("/")[0])

        def wait_for(predicate, description, *, healthy=False, timeout=60):
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                assert all(proc.poll() is None for proc in processes.values()), "daemon exited unexpectedly"
                if healthy:
                    sample()
                try:
                    if predicate():
                        return
                except (FileNotFoundError, ConnectionRefusedError):
                    pass
                time.sleep(1)
            raise AssertionError(f"timed out: {description}")

        def sample():
            rss, fds, state = {}, {}, {}
            maximum_latency = 0.0
            for node in "abc":
                proc = processes[node]
                assert proc.poll() is None, (node, proc.returncode)
                begin = time.monotonic()
                status = command(node, "status")
                maximum_latency = max(maximum_latency, (time.monotonic() - begin) * 1000)
                assert status["candidates"] <= 512, status
                # This scenario has only four route keys and four fixed identities.
                assert status["sources"] <= 16, status
                assert status["pending_requests"] <= 16, status
                peers = command(node, "neighbors")
                backbone = [peer for peer in peers if not (node == "b" and peer["interface"] == "babel2")]
                assert len(backbone) == (2 if node == "b" else 1), peers
                assert all(peer["reachable"] for peer in backbone), peers
                assert all(peer["candidates"] <= 256 for peer in peers), peers
                for interface in command(node, "interfaces"):
                    output = interface["output"]
                    assert 0 <= output["used_bytes"] <= output["budget_bytes"], output
                assert has_route(node, ANCHOR["a" if node != "a" else "c"]), node
                rss[node] = next(int(line.split()[1]) for line in Path(f"/proc/{proc.pid}/status").read_text().splitlines() if line.startswith("VmRSS:"))
                fds[node] = len(list(Path(f"/proc/{proc.pid}/fd").iterdir()))
                state[node] = {key: status[key] for key in ["candidates", "sources", "pending_requests"]}
            ping()
            entry = {"elapsed_seconds": round(time.monotonic() - started, 3),
                     "rss_kib": rss, "fds": fds, "state": state, "control_ms": round(maximum_latency, 3)}
            samples.append(entry)
            return entry

        try:
            for node, ns in namespaces.items():
                run("ip", "netns", "add", ns)
                created.append(ns)
                run("ip", "-n", ns, "link", "set", "lo", "up")
                run("ip", "netns", "exec", ns, "sysctl", "-qw", "net.ipv6.conf.all.forwarding=1")
                run("ip", "-n", ns, "-6", "rule", "add", "priority", "1000", "lookup", str(TABLE))
                if node in ANCHOR:
                    run("ip", "-n", ns, "-6", "addr", "add", ANCHOR[node], "dev", "lo")
            for index, other in enumerate("acd"):
                left, right = f"vkl{index}{os.getpid()}", f"vkr{index}{os.getpid()}"
                run("ip", "link", "add", left, "type", "veth", "peer", "name", right)
                links.extend([left, right])
                for device, node, interface, address in [
                    (left, other, "babel0", f"fe80::{other}"),
                    (right, "b", f"babel{index}", f"fe80::b{index}"),
                ]:
                    run("ip", "link", "set", device, "netns", namespaces[node], "name", interface)
                    run("ip", "-n", namespaces[node], "link", "set", interface, "addrgenmode", "none")
                    run("ip", "-n", namespaces[node], "-6", "addr", "add", f"{address}/64", "dev", interface, "nodad")
                    run("ip", "-n", namespaces[node], "link", "set", interface, "up")
            for node in NODES:
                config(node)
                start(node)
            wait_for(lambda: all(has_route(node, ANCHOR[other]) for node in NODES
                                 for other in "acd" if other != node), "initial convergence")
            started = time.monotonic()
            next_phase, next_report, phase = started, started, 0
            while time.monotonic() - started < args.seconds:
                for node, proc in processes.items():
                    assert proc.poll() is None, (node, proc.returncode)
                if time.monotonic() >= next_phase:
                    name = list(counts)[phase % len(counts)]
                    if name == "announce":
                        config("c", extra=True)
                        command("c", "reload")
                        wait_for(lambda: has_route("a", EXTRA), "new origin propagation", healthy=True)
                    elif name == "withdraw":
                        config("c")
                        command("c", "reload")
                        wait_for(lambda: not has_route("a", EXTRA), "origin withdrawal", healthy=True)
                    elif name == "link-down":
                        run("ip", "-n", namespaces["b"], "link", "set", "babel2", "down")
                        leaf_attached = False
                        wait_for(lambda: not has_route("a", ANCHOR["d"]), "leaf withdrawal after link loss", healthy=True)
                    elif name == "link-up":
                        restore_leaf_link()
                        leaf_attached = True
                        wait_for(lambda: has_route("a", ANCHOR["d"]), "leaf readmission", healthy=True)
                    else:
                        proc = processes.pop("d")
                        proc.terminate()
                        try:
                            status = proc.wait(timeout=8)
                            assert status == 0, ("leaf shutdown", status)
                        except BaseException:
                            processes["d"] = proc
                            raise
                        # Retain identity/checkpoint and change the advertised metric
                        # so an old route cannot satisfy the recovery assertion.
                        leaf_seq = (leaf_seq * 25173 + 13849) & 0xffff
                        metric = 100 + leaf_seq % 1000
                        config("d", metric=metric)
                        start("d")
                        wait_for(lambda: command("d", "status")["neighbors"] == 1
                                 and any(route["metric"] == metric + 192 for route in
                                         command("a", "routes", destination=ANCHOR["d"])["routes"]),
                                 "restarted leaf route propagation", healthy=True)
                    counts[name] += 1
                    phase += 1
                    # Fast smoke runs still cover every operation; long runs
                    # alternate phases every minute with steady traffic in between.
                    next_phase = time.monotonic() + min(60, args.seconds / 10)
                entry = sample()
                if time.monotonic() >= next_report:
                    print(json.dumps({"test": "steady-state", "progress": entry, "phases": counts}), flush=True)
                    next_report = time.monotonic() + 60
                time.sleep(1)
            if not leaf_attached:
                restore_leaf_link()
            config("c")
            command("c", "reload")
            wait_for(lambda: has_route("a", ANCHOR["d"]) and not has_route("a", EXTRA),
                     "final recovery", healthy=True)
            assert all(counts.values()), counts
            # Compare medians at each end, after the initial allocator warmup.
            # Short smoke runs record memory but do not claim a long-term plateau.
            memory = {}
            for node in "abc":
                warm = [s for s in samples if 180 <= s["elapsed_seconds"] < 300]
                tail = samples[-60:]
                if args.seconds >= 600:
                    assert warm, "missing warmup samples"
                    growth = statistics.median(s["rss_kib"][node] for s in tail) - statistics.median(s["rss_kib"][node] for s in warm)
                    assert growth <= args.rss_growth_kib, (node, "RSS growth", growth)
                    assert max(s["fds"][node] for s in tail) <= max(s["fds"][node] for s in warm) + 8, (node, "FD growth")
                else:
                    growth = None
                memory[node] = {"rss_peak_kib": max(s["rss_kib"][node] for s in samples), "median_growth_kib": growth}
            for proc in processes.values():
                proc.terminate()
            for node, proc in processes.items():
                assert proc.wait(timeout=8) == 0, (node, "shutdown")
            for node, ns in namespaces.items():
                assert not run("ip", "-n", ns, "-6", "route", "show", "table", str(TABLE), "proto", str(PROTOCOL)).strip(), (node, "stale owned route")
            print(json.dumps({"test": "steady-state", "result": "PASS", "seconds": args.seconds,
                              "seed": args.seed, "samples": len(samples), "phases": counts,
                              "max_control_ms": max(s["control_ms"] for s in samples), "memory": memory}), flush=True)
            success = True
        except BaseException:
            for node in processes:
                for operation in ["status", "interfaces", "neighbors", "routes"]:
                    try:
                        print(json.dumps({"node": node, "operation": operation,
                                          "diagnostic": command(node, operation)}), flush=True)
                    except Exception as error:
                        print(f"{node} {operation}: {error}", flush=True)
                result = subprocess.run(["ip", "-n", namespaces[node], "-6", "addr", "show"],
                                        text=True, capture_output=True, timeout=10)
                print(f"{node} addresses: {result.stdout}", flush=True)
            raise
        finally:
            for proc in processes.values():
                if proc.poll() is None:
                    proc.terminate()
            for proc in processes.values():
                try:
                    proc.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait(timeout=5)
            for log in logs:
                log.close()
                if not success:
                    print(f"--- {log.name} ---\n{Path(log.name).read_text()[-8000:]}", flush=True)
            for ns in reversed(created):
                subprocess.run(["ip", "netns", "del", ns], capture_output=True, timeout=10)
            for link in links:
                subprocess.run(["ip", "link", "del", link], capture_output=True, timeout=10)


if __name__ == "__main__":
    main()
