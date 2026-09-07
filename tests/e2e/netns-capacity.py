#!/usr/bin/env python3
"""Root-only A(flood)->B<-C(healthy), B->D isolation and recovery test.

The packet generator speaks Babel independently of our codec. Optional env:
BABEL_RS_CAPACITY_SECONDS (default 45), BABEL_RS_CAPACITY_PPS (default 250).
BABEL_RS_CAPACITY_PER_NEIGHBOR sets B's admission limit (default 256).
"""
import ipaddress
import json
import os
from pathlib import Path
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time


def inject():
    interface, offset, count, pps = sys.argv[2:]
    offset, count, pps = int(offset), int(count), int(pps)
    index = socket.if_nametoindex(interface)
    sock = socket.socket(socket.AF_INET6, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_BINDTODEVICE, interface.encode() + b"\0")
    sock.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_MULTICAST_IF, index)
    sock.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_MULTICAST_HOPS, 1)
    sock.bind(("fe80::a", 6696, 0, index))
    destination = ("ff02::1:6", 6696, 0, index)
    router_id = struct.pack("!BBH8s", 6, 10, 0, b"\xaa" * 8)
    next_hop = struct.pack("!BBBB8s", 7, 10, 3, 0, ipaddress.IPv6Address("fe80::a").packed[8:])
    updates = []
    for n in range(offset, offset + count):
        prefix = ipaddress.IPv6Address(f"2001:db8:bad::{n:x}").packed
        updates.append(struct.pack("!BBBBBBHHH", 8, 26, 2, 128, 128, 0, 100, 1, 100) + prefix)
    packets = [router_id + next_hop + b"".join(updates[n:n + 32]) for n in range(0, count, 32)]
    seqno = 0
    next_hello = 0
    n = 0
    while True:
        now = time.monotonic()
        body = packets[n % len(packets)]
        if now >= next_hello:
            seqno = (seqno + 1) & 65535
            body = (struct.pack("!BBHHH", 4, 6, 0, seqno, 20)
                    + struct.pack("!BBBBHH", 5, 6, 0, 0, 96, 60) + body)
            next_hello = now + 0.2
        sock.sendto(struct.pack("!BBH", 42, 2, len(body)) + body, destination)
        n += 1
        time.sleep(1 / pps)


def run(*args):
    result = subprocess.run(args, text=True, capture_output=True, timeout=10)
    if result.returncode:
        raise RuntimeError(f"{args}: {result.stdout}\n{result.stderr}")
    return result.stdout


def main():
    daemon = str(Path(sys.argv[1]).resolve())
    assert os.geteuid() == 0, "run on the designated root test VM"
    duration = int(os.environ.get("BABEL_RS_CAPACITY_SECONDS", "45"))
    pps = int(os.environ.get("BABEL_RS_CAPACITY_PPS", "250"))
    capacity = int(os.environ.get("BABEL_RS_CAPACITY_PER_NEIGHBOR", "256"))
    assert duration > 0 and pps > 0 and 1 <= capacity <= 4096
    namespaces = {n: f"vb-cap-{n}-{os.getpid()}" for n in "abcd"}
    processes = []
    created = []
    logs = []
    with tempfile.TemporaryDirectory(prefix="babel-rs-capacity.") as directory:
        runtime = Path(directory)

        def command(node, cmd, **params):
            with socket.socket(socket.AF_UNIX) as sock:
                sock.settimeout(3)
                sock.connect(str(runtime / f"{node}.ctl"))
                stream = sock.makefile("rwb")
                json.loads(stream.readline())
                stream.write((json.dumps({"api_version": 1, "id": 1, "command": cmd, "params": params}) + "\n").encode())
                stream.flush()
                response = json.loads(stream.readline())
                assert response["ok"], response
                return response["result"]

        def wait_for(predicate, description, timeout=20):
            until = time.monotonic() + timeout
            while time.monotonic() < until:
                assert all(p.poll() is None for p in processes), "test process exited"
                try:
                    if predicate():
                        return
                except (FileNotFoundError, ConnectionRefusedError):
                    pass
                time.sleep(0.2)
            raise AssertionError(description)

        def start(node, args):
            log = open(runtime / f"{node}-{len(logs)}.log", "w")
            logs.append(log)
            proc = subprocess.Popen(["ip", "netns", "exec", namespaces[node], *args], stdout=log, stderr=log)
            processes.append(proc)
            return proc

        def stop(proc):
            proc.terminate()
            proc.wait(timeout=5)
            processes.remove(proc)

        def config(node, extra=False):
            per_neighbor = capacity if node == "b" else max(1024, capacity * 2)
            origins = []
            if node in "cd":
                origins.append(f"2001:db8:{node}::1/128")
            if extra:
                origins.append("2001:db8:c::2/128")
            text = f'''router_id = "{':'.join([f'{ord(node):02x}'] * 8)}"
state_file = "{runtime}/{node}.state"
[limits]
max_neighbors = 8
max_candidates = 16384
max_candidates_per_neighbor = {per_neighbor}
[[interfaces]]
match = ["babel*"]
[interfaces.metric]
type = "wired"
'''
            for prefix in origins:
                text += f'[[origins]]\ndestination = "{prefix}"\n'
            text += '''[export]
protocol = 203
manage_rules = false
[[export.views]]
table = 25001
'''
            (runtime / f"{node}.toml").write_text(text)

        def has_route(node, prefix):
            return bool(command(node, "routes", destination=prefix)["routes"])

        def ping():
            run("ip", "netns", "exec", namespaces["d"], "ping", "-6", "-c", "1", "-W", "2", "-I", "2001:db8:d::1", "2001:db8:c::1")

        try:
            for node, ns in namespaces.items():
                run("ip", "netns", "add", ns)
                created.append(ns)
                run("ip", "-n", ns, "link", "set", "lo", "up")
                run("ip", "netns", "exec", ns, "sysctl", "-qw", "net.ipv6.conf.all.forwarding=1")
                run("ip", "-n", ns, "-6", "rule", "add", "priority", "1000", "lookup", "25001")
                if node in "cd":
                    run("ip", "-n", ns, "-6", "addr", "add", f"2001:db8:{node}::1/128", "dev", "lo")
            for index, node in enumerate("acd"):
                left, right = f"vcl{index}{os.getpid()}", f"vcr{index}{os.getpid()}"
                run("ip", "link", "add", left, "type", "veth", "peer", "name", right)
                for device, owner, interface, address in [
                    (left, node, "babel0", f"fe80::{node}"),
                    (right, "b", f"babel{index}", f"fe80::b{index}"),
                ]:
                    ns = namespaces[owner]
                    run("ip", "link", "set", device, "netns", ns, "name", interface)
                    run("ip", "-n", ns, "link", "set", interface, "addrgenmode", "none")
                    run("ip", "-n", ns, "-6", "addr", "add", f"{address}/64", "dev", interface, "nodad")
                    run("ip", "-n", ns, "link", "set", interface, "up")
            for node in "bcd":
                config(node)
                start(node, [daemon, "run", "--config", str(runtime / f"{node}.toml"), "--control-socket", str(runtime / f"{node}.ctl")])
            wait_for(lambda: has_route("d", "2001:db8:c::1/128"), "healthy multihop route")
            wait_for(lambda: has_route("c", "2001:db8:d::1/128"), "healthy return route")
            ping()
            flood = start("a", [sys.executable, str(Path(__file__).resolve()), "--inject", "babel0", "1", "8192", str(pps)])
            wait_for(lambda: command("b", "status")["rejected_candidates_per_neighbor"] > 0, "per-neighbor rejection")
            samples = []
            deadline = time.monotonic() + duration
            changed = False
            while time.monotonic() < deadline:
                begin = time.monotonic()
                status = command("b", "status")
                latency = time.monotonic() - begin
                peers = command("b", "neighbors")
                healthy = [p for p in peers if p["interface"] != "babel0"]
                assert len(healthy) == 2 and all(p["reachable"] for p in healthy), peers
                assert all(p["candidates"] <= capacity for p in peers), peers
                assert status["candidates"] <= 16384, status
                assert has_route("d", "2001:db8:c::1/128")
                ping()
                rss = {}
                for node, proc in zip("bcd", processes[:3]):
                    rss[node] = next(int(line.split()[1]) for line in Path(f"/proc/{proc.pid}/status").read_text().splitlines() if line.startswith("VmRSS:"))
                output = {}
                for node in "bcd":
                    output[node] = {}
                    for interface in command(node, "interfaces"):
                        usage = interface["output"]
                        assert usage["budget_bytes"] > 0, interface
                        assert 0 <= usage["used_bytes"] <= usage["budget_bytes"], interface
                        output[node][interface["name"]] = usage
                samples.append({"control_ms": latency * 1000, "rss_kib": rss, "candidates": status["candidates"], "sources": status["sources"], "output": output})
                if not changed:
                    config("c", extra=True)
                    command("c", "reload")
                    wait_for(lambda: has_route("d", "2001:db8:c::2/128"), "healthy new route during overload")
                    config("c")
                    command("c", "reload")
                    wait_for(lambda: not has_route("d", "2001:db8:c::2/128"), "healthy withdrawal during overload")
                    changed = True
                time.sleep(0.5)
            rejected = command("b", "status")["rejected_candidates_per_neighbor"]
            stop(flood)
            wait_for(lambda: command("b", "status")["neighbors"] == 2, "failed neighbor expiry")
            wait_for(lambda: command("b", "status")["candidates"] <= 3, "candidate budget release")
            # A completely new prefix set must be accepted without restarting B.
            start("a", [sys.executable, str(Path(__file__).resolve()), "--inject", "babel0", "32768", "16", "10"])
            wait_for(lambda: has_route("d", "2001:db8:bad::8000/128"), "multihop readmission after overload")
            ping()
            # Active resource limits cannot be changed through reload.
            before = command("b", "status")
            path = runtime / "b.toml"
            path.write_text(path.read_text().replace("max_candidates = 16384", "max_candidates = 16385"))
            try:
                command("b", "reload")
                raise AssertionError("limit reload was accepted")
            except AssertionError as error:
                assert "reload_rejected" in str(error), error
            after = command("b", "status")
            assert before["limits"] == after["limits"]
            assert before["config_generation"] == after["config_generation"]
            print(json.dumps({"test": "capacity-isolation", "result": "PASS", "seconds": duration,
                              "offered_packets_per_second": pps, "per_neighbor_limit": capacity, "rejected_updates": rejected,
                              "samples": samples}), flush=True)
        except Exception:
            for node in "bcd":
                for cmd, params in [("status", {}), ("neighbors", {}), ("routes", {"destination": "2001:db8:c::1/128"}), ("routes", {"destination": "2001:db8:d::1/128"})]:
                    try:
                        print(node, cmd, command(node, cmd, **params), file=sys.stderr)
                    except Exception as error:
                        print(node, cmd, error, file=sys.stderr)
                result = subprocess.run(["ip", "-n", namespaces[node], "-6", "route", "show", "table", "25001"], capture_output=True, text=True)
                print(node, result.stdout, file=sys.stderr)
            raise
        finally:
            for proc in reversed(processes):
                if proc.poll() is None:
                    proc.send_signal(signal.SIGTERM)
            for proc in reversed(processes):
                try:
                    proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait()
            for log in logs:
                log.close()
            if sys.exc_info()[0] is not None:
                for log in runtime.glob("*.log"):
                    print(log, log.read_text()[-6000:], file=sys.stderr)
            for ns in reversed(created):
                subprocess.run(["ip", "netns", "del", ns], check=False, capture_output=True)


if __name__ == "__main__":
    if sys.argv[1] == "--inject":
        inject()
    else:
        main()
