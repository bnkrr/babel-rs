#!/usr/bin/env python3
"""RFC 8967 independent interop, live rotation, and RFC 9079 transit forwarding."""
import importlib.util
import json
import os
from pathlib import Path
import signal
import sys
import time

spec = importlib.util.spec_from_file_location("boundaries", Path(__file__).with_name("netns-rfc-boundaries.py"))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
Lab = module.Lab


def configure(lab, node, origins=(), *, keys=("old",), algorithm="hmac_sha256", ipv4=False, managed=False):
    text = f'''router_id = "{node + 1:016x}"
state_file = "{lab.root}/{node}.state"
[[interfaces]]
match = ["lan*"]
control_transport = "{'ipv4' if ipv4 else 'ipv6'}"
hello_interval_ms = 500
update_interval_ms = 2000
[interfaces.metric]
type = "rtt"
'''
    if keys:
        text += '[interfaces.mac]\n'
        for key in keys:
            path = lab.root / (key + ".key")
            if not path.exists():
                path.write_text(("11" if key == "old" else "22") * 32 + "\n")
                path.chmod(0o600)
            text += f'[[interfaces.mac.keys]]\nalgorithm = "{algorithm}"\nkey_file = "{path}"\n'
    for dest, source in origins:
        text += f'[[origins]]\ndestination = "{dest}"\n'
        if source:
            text += f'source = "{source}"\n'
    text += f'[export]\nprotocol = 209\nmanage_rules = {str(managed).lower()}\n[[export.views]]\ntable = 201\n'
    (lab.root / f"{node}.toml").write_text(text)


def reload(lab, node):
    lab.control(node, "reload")
    # Reload publishes desired configuration; attachment is asynchronous.
    time.sleep(2.2)


def mac_pair(daemon, ipv4):
    with Lab(daemon, "mac4" if ipv4 else "mac6", 2) as lab:
        lab.link(0, 1, ipv4=ipv4, mtu=576 if ipv4 else 1280)
        for node in range(2):
            configure(lab, node, [(f"198.51.100.{node + 1}/32", None)], ipv4=ipv4)
            lab.start(node)
        lab.wait("mac-pair-ipv4" if ipv4 else "mac-pair-ipv6", lambda: lab.routes(0, 1) and lab.routes(1, 0))
        assert all(lab.control(n, "interfaces")[0]["mac_mode"] == "strict" for n in range(2))
        # Live overlap then remove old key, without restarting either process.
        for node in range(2):
            configure(lab, node, [(f"198.51.100.{node + 1}/32", None)], keys=("old", "new"), ipv4=ipv4)
            reload(lab, node)
        lab.wait("mac-rotation-overlap", lambda: lab.routes(0, 1) and lab.routes(1, 0))
        for node in range(2):
            configure(lab, node, [(f"198.51.100.{node + 1}/32", None)], keys=("new",), ipv4=ipv4)
            reload(lab, node)
        lab.wait("mac-rotation-new-only", lambda: lab.routes(0, 1) and lab.routes(1, 0))
        configure(lab, 1, [("198.51.100.2/32", None)], keys=("old",), ipv4=ipv4)
        reload(lab, 1)
        lab.wait("mac-wrong-key-isolated", lambda: not lab.control(1, "neighbors"))
        # Hold longer than an Update interval: no unsigned/wrong-key adjacency.
        time.sleep(3)
        assert not lab.control(1, "neighbors")
        configure(lab, 1, [("198.51.100.2/32", None)], keys=("new",), ipv4=ipv4)
        reload(lab, 1)
        lab.wait("mac-rekey-recovery", lambda: lab.routes(0, 1) and lab.routes(1, 0))
        # Restart one authenticator: old replay state must trigger a new challenge.
        old = lab.processes.pop(1)
        old.terminate(); old.wait(timeout=6)
        lab.start(1)
        lab.wait("mac-restart-recovery", lambda: lab.routes(0, 1) and lab.routes(1, 0))
        # Force authenticated multi-datagram updates at the small live MTU.
        origins = [("198.51.100.1/32", None)] + [(f"203.0.{i}.0/24", None) for i in range(96)]
        configure(lab, 0, origins, keys=("new",), ipv4=ipv4); reload(lab, 0)
        lab.wait("mac-mtu-complete-route-dump", lambda: lab.control(1, "status")["selected_routes"] == len(origins))
        budget = lab.control(0, "interfaces")[0]["udp_payload_budget"]
        assert budget == (548 if ipv4 else 1232) - 56, budget


def babeld_interop(daemon, algorithm):
    with Lab(daemon, "mac-babeld", 2) as lab:
        lab.link(0, 1, mtu=1280)
        configure(lab, 0, [("198.51.100.1/32", None)], algorithm=algorithm)
        lab.start(0)
        lab.ip(1, "route", "add", "blackhole", "198.51.100.2/32", "proto", "99")
        name = "hmac-sha256" if algorithm == "hmac_sha256" else "blake2s128"
        config = lab.root / "babeld.conf"
        config.write_text(f'''key id test type {name} value {'11' * 32}
interface lan key test hello-interval 0.5 update-interval 2
redistribute proto 99 allow
redistribute local deny
''')
        lab.start(1, ["babeld", "-d", "1", "-I", str(lab.root / "babeld.pid"), "-S", str(lab.root / "babeld.state"), "-t", "201", "-c", str(config), "lan"])
        lab.wait("mac-babeld-" + algorithm, lambda: lab.routes(0, 1) and lab.routes(1, 0))
        # Fresh advertisements after more than one update interval remain verified.
        time.sleep(3)
        assert lab.routes(0, 1) and lab.routes(1, 0)


def sadr_transit(daemon):
    with Lab(daemon, "sadr", 4) as lab:
        for node, name in [(1, "lanA"), (2, "lanB"), (3, "client")]:
            lab.link(0, node, left=name, right="lan")
        for node in range(4):
            lab.exec(node, "sysctl", "-qw", "net.ipv6.conf.all.forwarding=1")
        lab.ip(0, "addr", "add", "192.168.0.1/16", "dev", "client")
        lab.ip(0, "-6", "addr", "add", "2001:db8:1::1/48", "dev", "client", "nodad")
        for addr in ["192.168.1.5/16", "192.168.2.5/16"]:
            lab.ip(3, "addr", "add", addr, "dev", "lan")
        for addr in ["2001:db8:1:2::5/48", "2001:db8:1:3::5/48"]:
            lab.ip(3, "-6", "addr", "add", addr, "dev", "lan", "nodad")
        lab.ip(3, "route", "add", "default", "via", "192.168.0.1")
        lab.ip(3, "-6", "route", "add", "default", "via", "2001:db8:1::1")
        for node in [1, 2]:
            lab.ip(node, "-6", "route", "add", "default", "via", "fe80::1", "dev", "lan")
            lab.ip(node, "-4", "route", "add", "default", "via", "inet6", "fe80::1", "dev", "lan", "onlink")
            lab.ip(node, "addr", "add", "10.0.1.1/32", "dev", "lo")
            lab.ip(node, "-6", "addr", "add", "fd00:1::1/128", "dev", "lo", "nodad")
        lab.ip(2, "addr", "add", "198.51.100.1/32", "dev", "lo")
        lab.ip(2, "-6", "addr", "add", "fd00:2::1/128", "dev", "lo", "nodad")
        parent = [("10.0.0.0/16", "192.168.0.0/16"), ("fd00:1::/64", "2001:db8:1::/48")]
        child_default = [("0.0.0.0/0", "192.168.1.0/24"), ("::/0", "2001:db8:1:2::/64")]
        child_exact = [("10.0.0.0/16", "192.168.1.0/24"), ("fd00:1::/64", "2001:db8:1:2::/64")]
        for node, origins in [(0, []), (1, parent), (2, child_default)]:
            configure(lab, node, origins, managed=True)
            lab.start(node)

        def lookup(src, dst, device):
            family = "-6" if ":" in src else "-4"
            result = lab.ip(0, family, "-j", "route", "get", dst, "from", src, "iif", "client", check=False)
            return result.returncode == 0 and json.loads(result.stdout)[0].get("dev") == device

        cases = [("192.168.1.5", "10.0.1.1"), ("2001:db8:1:2::5", "fd00:1::1")]
        defaults = [("192.168.1.5", "198.51.100.1"), ("2001:db8:1:2::5", "fd00:2::1")]
        # Parent-prefix reachability does not imply the other neighbor's
        # source-specific default has arrived. Wait for both before transit.
        lab.wait("sadr-child-defaults-ready", lambda: all(lookup(s, d, "lanB") for s, d in defaults))
        lab.wait("sadr-destination-before-source", lambda: all(lookup(s, d, "lanA") for s, d in cases))
        for src, dst in cases + defaults:
            lab.exec(3, "ping", "-n", "-c", "1", "-W", "2", "-I", src, dst)
        configure(lab, 2, child_default + child_exact, managed=True); reload(lab, 2)
        lab.wait("sadr-equal-destination-more-specific-source", lambda: all(lookup(s, d, "lanB") for s, d in cases))
        assert lookup("192.168.2.5", "10.0.1.1", "lanA")
        assert lookup("2001:db8:1:3::5", "fd00:1::1", "lanA")
        for src, dst in cases:
            lab.exec(3, "ping", "-n", "-c", "1", "-W", "2", "-I", src, dst)
        configure(lab, 2, child_default, managed=True); reload(lab, 2)
        def unreachable(src, dst):
            family = "-6" if ":" in src else "-4"
            return lab.ip(0, family, "route", "get", dst, "from", src, "iif", "client", check=False).returncode != 0
        lab.wait("sadr-withdrawal-retains-unreachable-hold", lambda: all(unreachable(s, d) for s, d in cases))
        configure(lab, 2, child_default + child_exact, managed=True); reload(lab, 2)
        lab.wait("sadr-reannouncement-recovers", lambda: all(lookup(s, d, "lanB") for s, d in cases))
        # Static mode with no source views must filter these learned keys.
        config = lab.root / "0.toml"
        config.write_text(config.read_text().replace("manage_rules = true", "manage_rules = true\nautomatic_sources = false"))
        reload(lab, 0)
        lab.wait("sadr-static-uncovered-sources-filtered", lambda: lab.control(0, "status")["selected_routes"] == 0)
        assert len(lab.control(0, "neighbors")) == 2
        assert all(unreachable(s, d) for s, d in cases)
        configure(lab, 0, managed=True); reload(lab, 0)
        lab.wait("sadr-automatic-views-restored", lambda: all(lookup(s, d, "lanB") for s, d in cases))
        router = lab.processes.pop(0)
        router.terminate(); router.wait(timeout=6)
        for family in ["-4", "-6"]:
            assert not json.loads(lab.ip(0, family, "-j", "route", "show", "table", "all", "proto", "209").stdout)
            rules = json.loads(lab.ip(0, family, "-j", "rule", "show").stdout)
            assert not [r for r in rules if str(r.get("protocol")) == "209"], rules
        print(json.dumps({"phase": "sadr-owned-tables-rules-cleaned"}), flush=True)


if __name__ == "__main__":
    assert os.geteuid() == 0, "run in the privileged Linux test VM"
    daemon = str(Path(sys.argv[1]).resolve())
    modes = sys.argv[2:] or ["mac", "interop", "sadr"]
    assert set(modes) <= {"mac", "interop", "sadr"}, modes
    if "mac" in modes:
        for ipv4 in [False, True]:
            mac_pair(daemon, ipv4)
    if "interop" in modes:
        for algorithm in ["hmac_sha256", "blake2s128"]:
            babeld_interop(daemon, algorithm)
    if "sadr" in modes:
        sadr_transit(daemon)
