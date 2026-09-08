#!/usr/bin/env python3
"""Root-only partition/merge and concurrent path-loss/relay-crash regressions.

A--B--D is preferred, A--C--D is second, A--E--F--D is the fallback.
A<->E<->F is the unaffected forwarding path checked during every recovery poll.
Uses production Hello/Update timers, real kernel routes and the control API.
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


NODES = "abcdef"
ORIGINS = {node: f"2001:db8:{node}::1/128" for node in "adef"}
OLD = "2001:db8:d::2/128"
NEW = "2001:db8:d::3/128"
TABLE = 25301
PROTOCOL = 208
# name, left, right, nominal cost on both ends
LINKS = [("ab", "a", "b", 96), ("bd", "b", "d", 96),
         ("ac", "a", "c", 128), ("cd", "c", "d", 128),
         ("ae", "a", "e", 96), ("ef", "e", "f", 96),
         ("fd", "f", "d", 96)]


def run(*args):
    result = subprocess.run(args, capture_output=True, text=True, timeout=10)
    if result.returncode:
        raise RuntimeError(f"{args}: {result.stdout}\n{result.stderr}")
    return result.stdout


def main():
    daemon = str(Path(sys.argv[1]).resolve())
    assert os.geteuid() == 0 and os.access(daemon, os.X_OK)

    def interrupted(_signum, _frame):
        raise KeyboardInterrupt("test interrupted")

    signal.signal(signal.SIGTERM, interrupted)
    namespaces = {node: f"vb-combo-{node}-{os.getpid()}" for node in NODES}
    processes, created, temporary_links, logs = {}, [], [], []
    results = []
    healthy_samples = 0
    success = False
    with tempfile.TemporaryDirectory(prefix="babel-rs-combined.") as directory:
        runtime = Path(directory)

        def command(node, name, **params):
            with socket.socket(socket.AF_UNIX) as sock:
                sock.settimeout(3)
                sock.connect(str(runtime / f"{node}.ctl"))
                with sock.makefile("rwb") as stream:
                    json.loads(stream.readline())
                    stream.write((json.dumps({"api_version": 1, "id": 1,
                                              "command": name, "params": params}) + "\n").encode())
                    stream.flush()
                    reply = json.loads(stream.readline())
                    assert reply["ok"], reply
                    return reply["result"]

        def config(node, extra=OLD, table=TABLE):
            text = f'router_id = "{":".join([f"{ord(node):02x}"] * 8)}"\n'
            text += f'state_file = "{runtime}/{node}.state"\n'
            for name, left, right, cost in LINKS:
                if node in (left, right):
                    text += f'''[[interfaces]]
match = ["{name}"]
[interfaces.metric]
type = "wired"
nominal_cost = {cost}
'''
            if node in ORIGINS:
                text += f'[[origins]]\ndestination = "{ORIGINS[node]}"\n'
            if node == "d":
                text += f'[[origins]]\ndestination = "{extra}"\n'
            text += f'''[export]
protocol = {PROTOCOL}
manage_rules = false
[[export.views]]
table = {table}
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

        def routes(node, prefix):
            return command(node, "routes", destination=prefix)["routes"]

        def via(node, prefix, interface):
            return any(route["interface"] == interface for route in routes(node, prefix))

        def ping(source, destination):
            run("ip", "netns", "exec", namespaces[source], "ping", "-6", "-n", "-c", "1",
                "-W", "2", "-I", ORIGINS[source].split("/")[0], destination.split("/")[0])

        def healthy():
            nonlocal healthy_samples
            for node in "aef":
                status = command(node, "status")
                assert status["candidates"] <= status["limits"]["max_candidates"], status
                assert status["pending_requests"] <= 64, status
                assert status["export"]["last_error"] is None, status
                assert status["export"]["last_success_age_seconds"] < 10, status
                expected = {"a": {"ae"}, "e": {"ae", "ef"}, "f": {"ef"}}[node]
                peers = command(node, "neighbors")
                assert expected <= {p["interface"] for p in peers if p["reachable"]}, peers
            assert via("a", ORIGINS["f"], "ae") and via("f", ORIGINS["a"], "ef")
            ping("a", ORIGINS["f"])
            ping("f", ORIGINS["a"])
            healthy_samples += 1

        def wait_for(predicate, description, *, check_healthy=True, timeout=90):
            started = time.monotonic()
            while time.monotonic() - started < timeout:
                assert all(p.poll() is None for p in processes.values()), "unexpected daemon exit"
                if check_healthy:
                    healthy()
                try:
                    if predicate():
                        elapsed = round(time.monotonic() - started, 3)
                        results.append({"phase": description, "seconds": elapsed})
                        print(json.dumps(results[-1]), flush=True)
                        return
                except (FileNotFoundError, ConnectionRefusedError):
                    pass
                time.sleep(0.5)
            raise AssertionError(f"timed out: {description}")

        def path(source, destination):
            node, visited = source, []
            while node != destination:
                assert node not in visited, ("forwarding cycle", visited, node)
                visited.append(node)
                selected = routes(node, ORIGINS[destination])
                assert len(selected) == 1, (node, selected)
                interface = selected[0]["interface"]
                link = next(item for item in LINKS if item[0] == interface)
                node = link[2] if node == link[1] else link[1]
            return visited + [destination]

        def set_link(name, up):
            index = next(i for i, item in enumerate(LINKS) if item[0] == name)
            _, left, right, _ = LINKS[index]
            run("ip", "-n", namespaces[left], "link", "set", name, "up" if up else "down")
            if up:
                # Administrative down removes manual IPv6 link-local addresses.
                for side, node in enumerate((left, right), 1):
                    run("ip", "-n", namespaces[node], "-6", "addr", "replace",
                        f"fe80:{index + 1}::{side}/64", "dev", name, "nodad")

        def exported(node):
            status = command(node, "status")
            export = status["export"]
            return (export["last_error"] is None
                    and export["last_success_route_generation"] == status["route_generation"]
                    and export["last_success_config_generation"] == export["config_generation"])

        def active_kernel_route(node, prefix, table=TABLE):
            entries = json.loads(run("ip", "-n", namespaces[node], "-6", "-j", "route",
                                     "show", "table", str(table), "exact", prefix))
            return [entry for entry in entries if entry.get("type", "unicast") == "unicast"]

        try:
            for node, ns in namespaces.items():
                run("ip", "netns", "add", ns)
                created.append(ns)
                run("ip", "-n", ns, "link", "set", "lo", "up")
                run("ip", "netns", "exec", ns, "sysctl", "-qw", "net.ipv6.conf.all.forwarding=1")
                run("ip", "-n", ns, "-6", "rule", "add", "priority", "1000", "lookup", str(TABLE))
                if node in ORIGINS:
                    run("ip", "-n", ns, "-6", "addr", "add", ORIGINS[node], "dev", "lo")
            for prefix in (OLD, NEW):
                run("ip", "-n", namespaces["d"], "-6", "addr", "add", prefix, "dev", "lo")
            for index, (name, left, right, _) in enumerate(LINKS):
                devices = (f"cl{index}{os.getpid()}", f"cr{index}{os.getpid()}")
                run("ip", "link", "add", devices[0], "type", "veth", "peer", "name", devices[1])
                temporary_links.extend(devices)
                for side, (node, device) in enumerate(zip((left, right), devices), 1):
                    run("ip", "link", "set", device, "netns", namespaces[node], "name", name)
                    run("ip", "-n", namespaces[node], "link", "set", name, "addrgenmode", "none")
                    run("ip", "-n", namespaces[node], "-6", "addr", "add",
                        f"fe80:{index + 1}::{side}/64", "dev", name, "nodad")
                    run("ip", "-n", namespaces[node], "link", "set", name, "up")
            for node in NODES:
                config(node)
                start(node)
            wait_for(lambda: all(routes(node, ORIGINS[other]) for node in NODES
                                 for other in ORIGINS if other != node)
                     and via("a", ORIGINS["f"], "ae") and via("f", ORIGINS["a"], "ef")
                     and via("e", ORIGINS["d"], "ef") and via("f", ORIGINS["d"], "fd")
                     and all(exported(node) for node in NODES), "initial convergence", check_healthy=False)
            wait_for(lambda: via("a", ORIGINS["d"], "ab"), "preferred path")
            assert path("a", "d") == ["a", "b", "d"]
            ping("a", OLD)

            # Export config can change without a RIB change. Keep AEF forwarding
            # on TABLE while checking table migration on relay B.
            before = command("b", "status")
            config("b", table=TABLE + 1)
            command("b", "reload")
            wait_for(lambda: exported("b")
                     and active_kernel_route("b", ORIGINS["d"], TABLE + 1)
                     and not active_kernel_route("b", ORIGINS["d"]), "export table migration")
            after = command("b", "status")
            assert after["route_generation"] == before["route_generation"], (before, after)
            assert after["export"]["config_generation"] == before["export"]["config_generation"] + 1
            command("b", "reload")
            assert command("b", "status")["export"]["config_generation"] == after["export"]["config_generation"]
            config("b")
            command("b", "reload")
            wait_for(lambda: exported("b") and active_kernel_route("b", ORIGINS["d"]), "export table restored")

            # Partition D, then replace an origin while no path can propagate it.
            for name in ("bd", "cd", "fd"):
                set_link(name, False)
            wait_for(lambda: all(not routes(node, ORIGINS["d"]) and not routes(node, OLD)
                                 and not active_kernel_route(node, ORIGINS["d"])
                                 and not active_kernel_route(node, OLD) for node in "abcef"),
                     "partition removes stale paths")
            config("d", extra=NEW)
            command("d", "reload")
            assert not routes("a", NEW)
            for name in ("bd", "cd", "fd"):
                set_link(name, True)
            wait_for(lambda: routes("a", NEW) and routes("a", ORIGINS["d"])
                     and routes("d", ORIGINS["a"])
                     and all(not routes(node, OLD) and exported(node) for node in NODES),
                     "merge propagates new origin")
            ping("a", NEW)
            ping("d", ORIGINS["a"])
            assert not active_kernel_route("a", OLD)
            wait_for(lambda: via("a", ORIGINS["d"], "ab"), "preferred path after merge")
            assert path("a", "d") == ["a", "b", "d"]

            # Crash the standby relay and remove the primary link without a
            # convergence wait between faults. The third path must take over.
            crashed = processes.pop("c")
            crashed.kill()
            assert crashed.wait(timeout=5) == -signal.SIGKILL
            set_link("ab", False)
            wait_for(lambda: via("a", ORIGINS["d"], "ae")
                     and via("d", ORIGINS["a"], "fd")
                     and all(exported(node) for node in "aefd"),
                     "simultaneous primary loss and standby crash", timeout=240)
            assert path("a", "d") == ["a", "e", "f", "d"]
            ping("a", NEW)
            ping("d", ORIGINS["a"])
            assert active_kernel_route("a", ORIGINS["d"])[0]["dev"] == "ae"
            start("c")
            set_link("ab", True)
            wait_for(lambda: sum(peer["reachable"] for peer in command("c", "neighbors")) == 2
                     and via("c", ORIGINS["d"], "cd")
                     and via("a", ORIGINS["d"], "ab")
                     and via("d", ORIGINS["a"], "bd")
                     and all(exported(node) for node in NODES), "combined fault recovery", timeout=240)
            assert path("a", "d") == ["a", "b", "d"]
            ping("a", NEW)
            ping("d", ORIGINS["a"])
            assert not routes("a", OLD) and not active_kernel_route("a", OLD)

            for proc in processes.values():
                proc.terminate()
            for node, proc in processes.items():
                assert proc.wait(timeout=8) == 0, (node, "shutdown")
            for node, ns in namespaces.items():
                assert not run("ip", "-n", ns, "-6", "route", "show", "table", "all",
                               "proto", str(PROTOCOL)).strip(), (node, "stale owned route")
            print(json.dumps({"test": "combined-failures", "result": "PASS",
                              "healthy_samples": healthy_samples, "phases": results}), flush=True)
            success = True
        except BaseException:
            for node in processes:
                for operation in ("status", "neighbors", "routes"):
                    try:
                        print(json.dumps({"node": node, "operation": operation,
                                          "diagnostic": command(node, operation)}), flush=True)
                    except Exception as error:
                        print(f"{node} {operation}: {error}", flush=True)
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
            for device in temporary_links:
                subprocess.run(["ip", "link", "del", device], capture_output=True, timeout=10)


if __name__ == "__main__":
    main()
