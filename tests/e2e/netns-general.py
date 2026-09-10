#!/usr/bin/env python3
"""Finite shared-LAN IPv4/IPv6, live next-hop policy and asymmetric-loss E2E.

Two babel-rs instances and babeld share one bridge. Actual kernel gateways and
forwarding are checked; no route is accepted solely from a daemon's status.
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


def run(*args, check=True):
    return subprocess.run(args, check=check, text=True, capture_output=True, timeout=8)


def main():
    daemon = str(Path(sys.argv[1]).resolve())
    assert os.geteuid() == 0, "root required"
    token = f"vbg{os.getpid()}"
    spaces = [f"{token}-{i}" for i in range(3)]
    processes, handles, created = [], [], []
    bridge = token[:15]
    root_links = []
    phases = []

    def stop(*_):
        raise KeyboardInterrupt

    for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, stop)
    with tempfile.TemporaryDirectory(prefix="babel-general-") as directory:
        root = Path(directory)

        def ip(node, *args, check=True):
            return run("ip", "-n", spaces[node], *args, check=check)

        def control(node, command):
            with socket.socket(socket.AF_UNIX) as sock:
                sock.settimeout(2)
                sock.connect(str(root / f"{node}.ctl"))
                with sock.makefile("rwb") as stream:
                    json.loads(stream.readline())
                    stream.write((json.dumps({"api_version": 1, "id": 1, "command": command, "params": {}}) + "\n").encode())
                    stream.flush()
                    response = json.loads(stream.readline())
            assert response.get("ok"), response
            return response["result"]

        def config(node, mode="auto"):
            (root / f"{node}.toml").write_text(f'''router_id = "{node + 1:016x}"
state_file = "{root}/{node}.state"
[[interfaces]]
match = ["lan"]
ipv4_next_hop = "{mode}"
[[origins]]
destination = "198.51.100.{node + 1}/32"
[[origins]]
destination = "2001:db8:feed::{node + 1}/128"
[export]
protocol = 209
manage_rules = false
[[export.views]]
table = 201
''')

        def wait(label, predicate, budget=60):
            start = time.monotonic()
            last = None
            while time.monotonic() - start < budget:
                for proc in processes:
                    assert proc.poll() is None, f"daemon exited: {proc.returncode}"
                try:
                    if predicate():
                        phases.append({"phase": label, "seconds": round(time.monotonic() - start, 3)})
                        print(json.dumps(phases[-1]), flush=True)
                        return
                except (OSError, ValueError, AssertionError) as error:
                    last = str(error)
                time.sleep(0.25)
            raise AssertionError(f"{label} timed out: {last}")

        def route(node, peer, family=4):
            prefix = f"198.51.100.{peer + 1}/32" if family == 4 else f"2001:db8:feed::{peer + 1}/128"
            result = ip(node, f"-{family}", "-j", "route", "show", "table", "201", "exact", prefix, check=False)
            if result.returncode and "FIB table does not exist" in result.stderr:
                return None
            assert result.returncode == 0, result.stderr
            routes = json.loads(result.stdout)
            return next((r for r in routes if r.get("type", "unicast") == "unicast"), None)

        def gateway(node, peer, mode):
            r = route(node, peer)
            if not r:
                return False
            if mode == "ipv4":
                return r.get("gateway") == f"192.0.2.{peer + 1}"
            via = r.get("via", {})
            return via.get("host") == f"fe80::{peer + 1}" or r.get("gateway") == f"fe80::{peer + 1}"

        def ping(node, peer, family=4):
            dst = f"198.51.100.{peer + 1}" if family == 4 else f"2001:db8:feed::{peer + 1}"
            src = f"198.51.100.{node + 1}" if family == 4 else f"2001:db8:feed::{node + 1}"
            return run("ip", "netns", "exec", spaces[node], "ping", f"-{family}", "-n", "-c", "1", "-W", "1", "-I", src, dst, check=False).returncode == 0

        def all_routes():
            return all(route(a, b, family) for a in range(3) for b in range(3) if a != b for family in (4, 6))

        def reload_mode(mode):
            config(0, mode)
            processes[0].send_signal(signal.SIGHUP)
            wait("policy-" + mode, lambda: control(0, "interfaces")[0]["ipv4_next_hop"] == mode)

        try:
            run("ip", "link", "add", bridge, "type", "bridge", "mcast_snooping", "0")
            run("ip", "link", "set", bridge, "up")
            for node, ns in enumerate(spaces):
                run("ip", "netns", "add", ns)
                created.append(ns)
                left, right = f"vg{os.getpid()}{node}", f"vp{os.getpid()}{node}"
                run("ip", "link", "add", left, "type", "veth", "peer", "name", right)
                root_links.append(left)
                run("ip", "link", "set", left, "master", bridge)
                run("ip", "link", "set", left, "up")
                run("ip", "link", "set", right, "netns", ns, "name", "lan")
                ip(node, "link", "set", "lo", "up")
                ip(node, "link", "set", "lan", "addrgenmode", "none")
                ip(node, "addr", "add", f"192.0.2.{node + 1}/24", "dev", "lan")
                ip(node, "-6", "addr", "add", f"fe80::{node + 1}/64", "dev", "lan", "nodad")
                ip(node, "link", "set", "lan", "up")
                ip(node, "addr", "add", f"198.51.100.{node + 1}/32", "dev", "lo")
                ip(node, "-6", "addr", "add", f"2001:db8:feed::{node + 1}/128", "dev", "lo", "nodad")
                for family in (4, 6):
                    ip(node, f"-{family}", "rule", "add", "priority", "1000", "lookup", "201")
                if node in (0, 2):
                    config(node)
                    argv = [daemon, "run", "--config", str(root / f"{node}.toml"), "--control-socket", str(root / f"{node}.ctl")]
                else:
                    (root / "babeld.conf").write_text(f'''router-id 00:00:00:00:00:00:00:02
default type wired hello-interval 4 update-interval 16 v4-via-v6 false
redistribute ip 198.51.100.2/32 eq 32 allow
redistribute ip 2001:db8:feed::2/128 eq 128 allow
redistribute deny
''')
                    argv = ["babeld", "-c", str(root / "babeld.conf"), "-S", str(root / "babeld.state"), "-I", str(root / "babeld.pid"), "-t", "201", "lan"]
                log = open(root / f"{node}.log", "w")
                handles.append(log)
                processes.append(subprocess.Popen(["ip", "netns", "exec", ns, *argv], stdout=log, stderr=log))

            wait("dual-stack-shared-lan", lambda: all_routes() and control(0, "status")["neighbors"] == 2)
            assert all(gateway(a, b, "ipv4") for a in range(3) for b in range(3) if a != b)
            assert all(ping(a, b, family) for a in range(3) for b in range(3) if a != b for family in (4, 6))
            reload_mode("ipv6")
            # babeld 1.13.1 retains its original next hop on an existing route
            # even after a new AE/Next-Hop update. Validate live switching on C;
            # record B's behavior without treating it as proof of a successful switch.
            wait("forced-ipv6-next-hop", lambda: gateway(2, 0, "ipv6"))
            print(json.dumps({"peer-observation": "babeld-after-live-mode-change", "route": route(1, 0)}), flush=True)
            assert ping(2, 0) and ping(0, 2)
            reload_mode("auto")
            wait("auto-prefers-ipv4", lambda: gateway(1, 0, "ipv4") and gateway(2, 0, "ipv4"))
            ip(0, "addr", "del", "192.0.2.1/24", "dev", "lan")
            wait("auto-address-removal", lambda: gateway(2, 0, "ipv6"))
            assert ping(2, 0) and ping(0, 1, 6)
            reload_mode("ipv4")
            wait("forced-ipv4-unavailable", lambda: control(0, "interfaces")[0]["effective_ipv4_next_hop"] == "unavailable" and route(1, 0) is None and route(2, 0) is None)
            assert ping(0, 1, 6) and ping(1, 0, 6)
            ip(0, "addr", "add", "192.0.2.1/24", "dev", "lan")
            wait("forced-ipv4-address-restored", lambda: gateway(1, 0, "ipv4") and gateway(2, 0, "ipv4"))
            assert ping(1, 0)

            # One-way loss: C cannot transmit; A<->B must continue forwarding.
            run("ip", "netns", "exec", spaces[2], "tc", "qdisc", "add", "dev", "lan", "root", "netem", "loss", "100%")
            start = time.monotonic()
            while time.monotonic() - start < 18:
                assert ping(0, 1) and ping(1, 0) and ping(0, 1, 6)
                time.sleep(0.25)
            assert route(0, 2) is None and route(1, 2) is None
            phases.append({"phase": "one-way-loss-healthy-peers", "seconds": round(time.monotonic() - start, 3)})
            run("ip", "netns", "exec", spaces[2], "tc", "qdisc", "del", "dev", "lan", "root")
            wait("one-way-loss-recovery", all_routes)
            assert ping(2, 0) and ping(0, 2)
            run("ip", "netns", "exec", spaces[2], "tc", "qdisc", "add", "dev", "lan", "root", "netem", "loss", "10%")
            time.sleep(12)
            assert all_routes()
            run("ip", "netns", "exec", spaces[2], "tc", "qdisc", "del", "dev", "lan", "root")
            wait("packet-loss-recovery", lambda: all_routes() and ping(2, 0) and ping(0, 2))
            print(json.dumps({"test": "general", "result": "PASS", "phases": phases}), flush=True)
        except BaseException:
            for node, ns in enumerate(created):
                log = root / f"{node}.log"
                if log.is_file():
                    print(f"node {node}: {log.read_text()[-5000:]}", file=sys.stderr)
                print(run("ip", "-n", ns, "-4", "route", "show", "table", "all", check=False).stdout, file=sys.stderr)
                print(run("ip", "-n", ns, "-6", "route", "show", "table", "all", check=False).stdout, file=sys.stderr)
            raise
        finally:
            for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
                signal.signal(sig, signal.SIG_IGN)
            for proc in processes:
                if proc.poll() is None:
                    proc.terminate()
            for proc in processes:
                try:
                    proc.wait(timeout=7)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait(timeout=2)
            for handle in handles:
                handle.close()
            for link in root_links:
                run("ip", "link", "del", link, check=False)
            for ns in created:
                run("ip", "netns", "del", ns)
            run("ip", "link", "del", bridge, check=False)


if __name__ == "__main__":
    main()
