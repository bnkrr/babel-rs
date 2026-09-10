#!/usr/bin/env python3
"""Finite IPv4 control, independent RTT and unnumbered ICMPv4 regressions."""
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
    return subprocess.run(args, check=check, text=True, capture_output=True, timeout=10)


class Lab:
    def __init__(self, daemon, name, count):
        self.daemon = daemon
        self.temp = tempfile.TemporaryDirectory(prefix="babel-rfc-")
        self.root = Path(self.temp.name)
        self.spaces = [f"vbrfc-{os.getpid()}-{name}-{i}" for i in range(count)]
        self.created, self.processes, self.logs = [], [], []

    def __enter__(self):
        try:
            for i, name in enumerate(self.spaces):
                run("ip", "netns", "add", name)
                self.created.append(name)
                self.ip(i, "link", "set", "lo", "up")
                self.exec(i, "sysctl", "-qw", "net.ipv4.ip_forward=1", "net.ipv4.conf.all.rp_filter=0", "net.ipv4.conf.default.rp_filter=0")
            return self
        except BaseException:
            self.__exit__(*sys.exc_info())
            raise

    def __exit__(self, kind, value, traceback):
        for proc in self.processes:
            if proc.poll() is None:
                proc.terminate()
        for proc in self.processes:
            try:
                proc.wait(timeout=6)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
        for log in self.logs:
            log.close()
        if kind:
            for log in self.root.glob("*.log"):
                print(log.name, log.read_text()[-12000:], file=sys.stderr)
        for name in reversed(self.created):
            run("ip", "netns", "del", name, check=False)
        self.temp.cleanup()

    def ip(self, node, *args, check=True):
        return run("ip", "-n", self.spaces[node], *args, check=check)

    def exec(self, node, *args, check=True):
        return run("ip", "netns", "exec", self.spaces[node], *args, check=check)

    def link(self, a, b, left="lan", right="lan", ipv4=False, mtu=1500):
        first, second = f"rf{os.getpid()}a", f"rf{os.getpid()}b"
        run("ip", "link", "add", first, "type", "veth", "peer", "name", second)
        try:
            run("ip", "link", "set", first, "netns", self.spaces[a], "name", left)
            run("ip", "link", "set", second, "netns", self.spaces[b], "name", right)
        finally:
            run("ip", "link", "del", first, check=False)
        for node, interface in [(a, left), (b, right)]:
            self.ip(node, "link", "set", interface, "addrgenmode", "none")
            self.ip(node, "link", "set", interface, "mtu", str(mtu), "up")
            if ipv4:
                self.ip(node, "addr", "add", f"192.0.2.{node + 1}/24", "dev", interface)
            else:
                self.ip(node, "-6", "addr", "add", f"fe80::{node + 1}/64", "dev", interface, "nodad")

    def config(self, node, transport="ipv6", rtt=False, origin=True):
        text = f'''router_id = "{node + 1:016x}"
state_file = "{self.root}/{node}.state"
[[interfaces]]
match = ["lan*"]
control_transport = "{transport}"
hello_interval_ms = 1000
update_interval_ms = 4000
'''
        if rtt:
            text += '[interfaces.metric]\ntype = "rtt"\n'
        if origin:
            text += f'[[origins]]\ndestination = "198.51.100.{node + 1}/32"\n'
        text += '[export]\nprotocol = 209\nmanage_rules = false\n[[export.views]]\ntable = 201\n'
        (self.root / f"{node}.toml").write_text(text)

    def start(self, node, argv=None):
        if argv is None:
            argv = [self.daemon, "run", "--config", str(self.root / f"{node}.toml"), "--control-socket", str(self.root / f"{node}.ctl")]
        log = open(self.root / f"{node}.log", "w")
        self.logs.append(log)
        proc = subprocess.Popen(["ip", "netns", "exec", self.spaces[node], *argv], stdout=log, stderr=log)
        self.processes.append(proc)
        return proc

    def control(self, node, command):
        with socket.socket(socket.AF_UNIX) as sock:
            sock.settimeout(2)
            sock.connect(str(self.root / f"{node}.ctl"))
            with sock.makefile("rwb") as stream:
                json.loads(stream.readline())
                stream.write((json.dumps({"api_version": 1, "id": 1, "command": command, "params": {}}) + "\n").encode())
                stream.flush()
                reply = json.loads(stream.readline())
        assert reply.get("ok"), reply
        return reply["result"]

    def wait(self, label, predicate, seconds=35):
        start = time.monotonic()
        last = None
        while time.monotonic() - start < seconds:
            assert all(p.poll() is None for p in self.processes), "peer exited"
            try:
                if predicate():
                    print(json.dumps({"phase": label, "seconds": round(time.monotonic() - start, 3)}), flush=True)
                    return
            except (OSError, ValueError, AssertionError, IndexError) as error:
                last = str(error)
            time.sleep(0.2)
        raise AssertionError(f"{label}: {last}")

    def routes(self, node, peer):
        result = self.ip(node, "-j", "route", "show", "table", "201", "exact", f"198.51.100.{peer + 1}/32", check=False)
        return result.returncode == 0 and any(r.get("type", "unicast") == "unicast" for r in json.loads(result.stdout))


def ipv4_control(daemon):
    with Lab(daemon, "v4", 2) as lab:
        for i in range(2):
            lab.exec(i, "sysctl", "-qw", "net.ipv6.conf.all.disable_ipv6=1", "net.ipv6.conf.default.disable_ipv6=1")
        lab.link(0, 1, ipv4=True, mtu=576)
        for i in range(2):
            lab.ip(i, "addr", "add", f"198.51.100.{i + 1}/32", "dev", "lo")
            lab.ip(i, "rule", "add", "priority", "1000", "lookup", "201")
            lab.config(i, transport="ipv4")
            lab.start(i)
        lab.wait("ipv4-only-control-mtu576", lambda: lab.routes(0, 1) and lab.routes(1, 0))
        for i in range(2):
            assert lab.ip(i, "-6", "-j", "addr", "show").stdout.strip() == "[]"
            details = lab.control(i, "interfaces")[0]
            assert details["control_transport"] == "ipv4" and details["udp_payload_budget"] == 548, details
            lab.exec(i, "ping", "-n", "-c", "1", "-W", "2", "-I", f"198.51.100.{i + 1}", f"198.51.100.{2 - i}")
        # Validly framed Hellos with wrong port / off-subnet source must not
        # allocate a second adjacency on the receiving interface.
        injection = """import socket
for address, port in [('198.51.100.2', 6696), ('192.0.2.2', 16696)]:
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        sock.bind((address, port))
        sock.sendto(bytes.fromhex('2a0200080406000000010190'), ('192.0.2.1', 6696))
"""
        lab.exec(1, "python3", "-c", injection)
        time.sleep(0.3)
        peers = lab.control(0, "neighbors")
        assert len(peers) == 1 and peers[0]["address"] == "192.0.2.2", peers
        print(json.dumps({"phase": "ipv4-source-admission", "neighbors": len(peers)}), flush=True)
        # A live family change must replace sockets and old adjacency state.
        for i in range(2):
            lab.ip(i, "link", "set", "lan", "mtu", "1500")
            lab.exec(i, "sysctl", "-qw", "net.ipv6.conf.all.disable_ipv6=0", "net.ipv6.conf.default.disable_ipv6=0")
            lab.ip(i, "-6", "addr", "add", f"fe80::{i + 1}/64", "dev", "lan", "nodad")
            lab.config(i)
            lab.processes[i].send_signal(signal.SIGHUP)
        lab.wait("control-family-reload", lambda: all(lab.control(i, "interfaces")[0]["control_transport"] == "ipv6" for i in range(2)) and lab.routes(0, 1) and lab.routes(1, 0))


def independent_rtt(daemon):
    with Lab(daemon, "rtt", 2) as lab:
        lab.link(0, 1)
        for i in range(2):
            lab.exec(i, "tc", "qdisc", "add", "dev", "lan", "root", "netem", "delay", "20ms")
        lab.config(0, rtt=True, origin=False)
        lab.start(0)
        config = lab.root / "babeld.conf"
        config.write_text('router-id 00:00:00:00:00:00:00:02\ndefault type wired hello-interval 1 update-interval 4 enable-timestamps true rtt-min 10 rtt-max 120 max-rtt-penalty 150\nredistribute deny\n')
        lab.start(1, ["babeld", "-d", "1", "-c", str(config), "-S", str(lab.root / "peer.state"), "-I", str(lab.root / "peer.pid"), "lan"])
        def sampled():
            peers = lab.control(0, "neighbors")
            return len(peers) == 1 and peers[0]["reachable"] and 25_000 <= (peers[0].get("smoothed_rtt_us") or 0) <= 150_000 and peers[0].get("rtt_penalty", 0) > 0
        lab.wait("independent-babeld-rtt-40ms", sampled)
        print(json.dumps({"rtt-peer": lab.control(0, "neighbors")}), flush=True)


def icmp_unnumbered(daemon):
    with Lab(daemon, "icmp", 3) as lab:
        lab.link(0, 1, right="lan0")
        lab.link(1, 2, left="lan1", mtu=1280)
        for i in range(3):
            lab.ip(i, "rule", "add", "priority", "1000", "lookup", "201")
            if i != 1:
                lab.ip(i, "addr", "add", f"198.51.100.{i + 1}/32", "dev", "lo")
            lab.config(i, origin=i != 1)
            lab.start(i)
        lab.wait("ipv4-via-ipv6-forwarding", lambda: all(lab.routes(a, b) for a, b in [(0, 2), (1, 0), (1, 2), (2, 0)]))
        lab.exec(0, "ping", "-n", "-c", "1", "-W", "2", "-I", "198.51.100.1", "198.51.100.3")
        for numbered in [True, False]:
            if numbered:
                lab.ip(1, "addr", "add", "198.51.100.2/32", "dev", "lo")
            else:
                lab.ip(1, "addr", "del", "198.51.100.2/32", "dev", "lo")
            for name, args, expected in [
                ("ttl", ["-t", "1"], "Time to live exceeded"),
                ("pmtu", ["-M", "probe", "-s", "1400"], "Frag needed and DF set"),
            ]:
                result = lab.exec(0, "ping", "-n", "-c", "1", "-W", "2", "-I", "198.51.100.1", *args, "198.51.100.3", check=False)
                output = result.stdout + result.stderr
                assert expected.lower() in output.lower(), output
                if not numbered:
                    assert "192.0.0.8" in output, output
                print(json.dumps({"phase": name, "router-has-ipv4": numbered, "result": output.strip()}), flush=True)


def main():
    assert os.geteuid() == 0, "root required"
    daemon = str(Path(sys.argv[1]).resolve())
    for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, lambda *_: (_ for _ in ()).throw(KeyboardInterrupt()))
    ipv4_control(daemon)
    independent_rtt(daemon)
    icmp_unnumbered(daemon)


if __name__ == "__main__":
    main()
