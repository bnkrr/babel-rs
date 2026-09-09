#!/usr/bin/env python3
"""Opt-in endless Linux netns test. Never invoked by the normal suite or CI."""
import argparse
from collections import deque
import hashlib
import json
import logging
from logging.handlers import RotatingFileHandler
import os
from pathlib import Path
import shutil
import signal
import socket
import statistics
import subprocess
import tempfile
import threading
import time

from model import Topology, NotConverged, address, audit, link_local, prefix
from instances import assign_instances, foreign_command, launch, parse_mix


# babeld's Linux exporter may truncate table IDs to eight bits. Namespaces
# already isolate each instance, so a low table works for every implementation.
TABLE, PROTOCOL = 201, 209


class StopRequested(BaseException):
    pass


def arguments():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("daemon", nargs="?", type=Path)
    parser.add_argument("--nodes", "--max-nodes", dest="nodes", type=int, default=8,
                        help="maximum/full node pool, 3..64; starts fully populated")
    parser.add_argument("--min-nodes", type=int, default=2, help="minimum active nodes, 2..nodes")
    parser.add_argument("--mix", default="babel-rs=1", help="per-slot sampling weights, e.g. babel-rs=2,bird=1,babeld=1")
    parser.add_argument("--bird", type=Path, default=Path("bird"), help="BIRD executable on the test host")
    parser.add_argument("--babeld", type=Path, default=Path("babeld"), help="babeld executable on the test host")
    parser.add_argument("--avg-degree", type=float, help="initial 2E/N, rounded to the nearest edge; default min(3, graph maximum)")
    parser.add_argument("--graph", choices=("mesh", "bottleneck", "hub"), default="mesh")
    parser.add_argument("--seed", type=int, default=1)
    parser.add_argument("--on-failure", choices=("stop", "next-seed"), default="stop",
                        help="stop, or archive each failure and start fresh with seed + 1")
    parser.add_argument("--rounds", type=int, default=0, help="0 loops until interrupted or failed")
    parser.add_argument("--changes", type=int, default=2, help="random operations per round, 1..10")
    parser.add_argument("--settle-timeout", type=float,
                        help="phase deadline; default 300 seconds, or 600 when mixing foreign daemons")
    parser.add_argument("--stable-seconds", type=float, default=10, help="continuous audit success required before the next round")
    parser.add_argument("--probe-pairs", type=int, default=16, help="rotating reachable pairs per audit; all RIB/FIB pairs are always checked")
    parser.add_argument("--rss-growth-mib", type=int, default=64, help="allowed rolling-median RSS growth after five minutes per process")
    parser.add_argument("--artifacts", type=Path, help="new directory; default .local/experiments/endless/<time>-<pid>")
    parser.add_argument("--plan", action="store_true", help="print a finite event plan without root, network or artifacts")
    parser.add_argument("--validate", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    try:
        weights = parse_mix(args.mix)
    except ValueError as error:
        parser.error(str(error))
    if args.settle_timeout is None:
        args.settle_timeout = 600 if weights["bird"] or weights["babeld"] else 300
    import math
    if (args.rounds < 0 or not 1 <= args.changes <= 10 or args.probe_pairs < 1
            or args.rss_growth_mib < 1 or not math.isfinite(args.settle_timeout)
            or not math.isfinite(args.stable_seconds)
            or not 0 < args.stable_seconds < args.settle_timeout):
        parser.error("invalid round/change/probe/resource bounds or stable/settle durations")
    try:
        topology = Topology(args.nodes, args.avg_degree, args.graph, args.seed, min_nodes=args.min_nodes)
    except ValueError as error:
        parser.error(str(error))
    if args.plan:
        if args.rounds == 0:
            parser.error("--plan requires a positive --rounds")
    elif not args.validate and (args.daemon is None or not os.access(args.daemon, os.X_OK)):
        parser.error("an executable daemon is required")
    return args, topology


def rotating_logger(path, size, backups):
    logger = logging.Logger(str(path))
    handler = RotatingFileHandler(path, maxBytes=size, backupCount=backups)
    handler.setFormatter(logging.Formatter("%(message)s"))
    logger.addHandler(handler)
    return logger


class Runner:
    def __init__(self, args, topology, runtime, artifacts):
        self.args, self.topology = args, topology
        self.runtime, self.artifacts = runtime, artifacts
        self.daemon = str(args.daemon.resolve())
        self.implementations = assign_instances(args.nodes, args.seed, getattr(args, "mix", "babel-rs=1"))
        self.binaries = {"babel-rs": self.daemon,
                         "bird": shutil.which(str(getattr(args, "bird", "bird"))),
                         "babeld": shutil.which(str(getattr(args, "babeld", "babeld")))}
        self.token = f"{os.getpid() & 0xffff:04x}{os.urandom(2).hex()}"
        self.nodes, self.links, self.created, self.temporary_links = {}, set(), set(), set()
        self.round = 0
        self.counts = {name: 0 for name in ("add-node", "delete-node", "link-up", "link-down")}
        self.started = time.monotonic()
        self.deadline = None
        self.probe_cursor = 0
        self.latest = {}
        self.verification = {}
        self.initial_verified = False
        self.events = rotating_logger(artifacts / "events.jsonl", 4 * 1024 * 1024, 3)
        self.samples = rotating_logger(artifacts / "samples.jsonl", 1024 * 1024, 1)
        self.node_logs = {}

    def record(self, kind, **fields):
        entry = {"kind": kind, "round": self.round, "elapsed": round(time.monotonic() - self.started, 3), **fields}
        self.events.info(json.dumps(entry))
        try:
            print(json.dumps(entry), flush=True)
        except OSError:
            pass  # Retain file records and cleanup if the SSH terminal closes.

    def timeout(self, maximum):
        if self.deadline is None:
            return maximum
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("phase deadline exceeded")
        return min(maximum, remaining)

    def run(self, *args, check=True):
        try:
            result = subprocess.run(args, capture_output=True, text=True, timeout=self.timeout(5),
                                    env={**os.environ, "LC_ALL": "C"})
        except subprocess.TimeoutExpired as error:
            if self.deadline is not None and time.monotonic() >= self.deadline:
                raise TimeoutError(f"phase deadline exceeded during {args!r}; "
                                   f"last rejection: {self.verification.get('last_rejection')}") from error
            raise
        if check and result.returncode:
            raise RuntimeError(f"{args}: {result.stdout}\n{result.stderr}")
        return result

    def namespace(self, node):
        return f"vbe-{self.token}-{node}"

    def command(self, node, name):
        kind = self.implementations[node]
        if kind != "babel-rs":
            operation = "dump" if kind == "babeld" else {
                "status": "show status", "routes": "show route table test6 all",
                "interfaces": "show babel interfaces", "neighbors": "show babel neighbors"}[name]
            return foreign_command(kind, self.runtime / f"{node}.ctl", operation, self.timeout(3))
        with socket.socket(socket.AF_UNIX) as sock:
            sock.settimeout(self.timeout(3))
            sock.connect(str(self.runtime / f"{node}.ctl"))
            with sock.makefile("rwb") as stream:
                greeting = stream.readline(1024 * 1024 + 1)
                if len(greeting) > 1024 * 1024:
                    raise RuntimeError(f"node {node}: oversized control greeting")
                json.loads(greeting)
                stream.write((json.dumps({"api_version": 1, "id": 1, "command": name, "params": {}}) + "\n").encode())
                stream.flush()
                # Bound the verifier too, rather than trusting a daemon response.
                line = stream.readline(1024 * 1024 + 1)
                if len(line) > 1024 * 1024:
                    raise RuntimeError(f"node {node}: oversized control response")
                reply = json.loads(line)
                if not reply["ok"]:
                    raise RuntimeError(f"node {node}: {reply}")
                return reply["result"]

    def add_node(self, node):
        ns = self.namespace(node)
        self.run("ip", "netns", "add", ns)
        self.created.add(node)
        self.run("ip", "-n", ns, "link", "set", "lo", "up")
        self.run("ip", "-n", ns, "-6", "addr", "add", prefix(node), "dev", "lo")
        self.run("ip", "netns", "exec", ns, "sysctl", "-qw", "net.ipv6.conf.all.forwarding=1")
        self.run("ip", "-n", ns, "-6", "rule", "add", "priority", "1000", "lookup", str(TABLE))
        kind = self.implementations[node]
        command = launch(kind, self.binaries[kind], node, self.runtime,
                         self.topology.edges, TABLE, PROTOCOL)
        logger = self.node_logs.get(node)
        if logger is None:
            logger = rotating_logger(self.artifacts / f"node-{node}.log", 1024 * 1024, 1)
            self.node_logs[node] = logger
        proc = subprocess.Popen(["ip", "netns", "exec", ns, *command],
                                stdout=subprocess.PIPE, stderr=subprocess.STDOUT)

        def drain():
            with proc.stdout:
                while chunk := proc.stdout.readline(65536):
                    logger.info(chunk.decode(errors="replace").rstrip("\n"))

        thread = threading.Thread(target=drain, daemon=True)
        thread.start()
        self.nodes[node] = {"proc": proc, "thread": thread, "started": time.monotonic(),
                            "rss": deque(maxlen=60), "baseline": None, "ready": False}
        self.record("node-start", node=node, implementation=kind, pid=proc.pid)
        for index, edge in enumerate(self.topology.edges):
            if node in edge and set(edge) <= self.nodes.keys():
                self.add_link(index)

    def add_link(self, index):
        if index in self.links:
            return
        edge = self.topology.edges[index]
        left, right = f"vl{index:x}{self.token}", f"vr{index:x}{self.token}"
        self.run("ip", "link", "add", left, "type", "veth", "peer", "name", right)
        self.temporary_links.update((left, right))
        for node, device in zip(edge, (left, right)):
            ns = self.namespace(node)
            self.run("ip", "link", "set", device, "netns", ns, "name", f"e{index}")
            self.temporary_links.discard(device)
            self.run("ip", "-n", ns, "link", "set", f"e{index}", "addrgenmode", "none")
            self.run("ip", "-n", ns, "-6", "addr", "add", link_local(node) + "/64", "dev", f"e{index}", "nodad")
            self.run("ip", "-n", ns, "link", "set", f"e{index}", "up")
        self.links.add(index)
        if index not in self.topology.enabled:
            self.set_link(index, False)

    def set_link(self, index, up):
        a, b = self.topology.edges[index]
        self.run("ip", "-n", self.namespace(a), "link", "set", f"e{index}", "up" if up else "down")
        if up:
            for node in (a, b):
                self.run("ip", "-n", self.namespace(node), "-6", "addr", "replace", link_local(node) + "/64",
                         "dev", f"e{index}", "nodad")

    def delete_node(self, node):
        info = self.nodes[node]
        proc = info["proc"]
        proc.kill()  # Removal is abrupt; the retained identity is reused on add.
        proc.wait(timeout=self.timeout(5))
        info["thread"].join(timeout=3)
        if info["thread"].is_alive():
            raise RuntimeError(f"node {node}: log drain did not stop")
        del self.nodes[node]
        # Namespace destruction can lag behind unlinking its name (or an open
        # reference can keep it alive). Remove veth pairs synchronously before
        # reusing their names in surviving peers during an immediate re-add.
        for index in sorted(self.links.copy()):
            if node in self.topology.edges[index]:
                self.run("ip", "-n", self.namespace(node), "link", "del", f"e{index}")
                self.links.remove(index)
        self.run("ip", "netns", "del", self.namespace(node))
        self.created.remove(node)

    def apply(self, event):
        operation, target = event["operation"], event["target"]
        self.record("operation", **event)
        if operation == "add-node":
            self.add_node(target)
        elif operation == "delete-node":
            self.delete_node(target)
        else:
            self.set_link(target, operation == "link-up")
        self.counts[operation] += 1

    def resources(self, node, status):
        info = self.nodes[node]
        proc = info["proc"]
        lines = Path(f"/proc/{proc.pid}/status").read_text().splitlines()
        rss = next(int(line.split()[1]) for line in lines if line.startswith("VmRSS:"))
        fds = len(list(Path(f"/proc/{proc.pid}/fd").iterdir()))
        info["rss"].append(rss)
        median = statistics.median(info["rss"])
        if time.monotonic() - info["started"] >= 300 and len(info["rss"]) == 60:
            if info["baseline"] is None:
                info["baseline"] = median
            elif median > info["baseline"] + self.args.rss_growth_mib * 1024:
                raise RuntimeError(f"node {node}: median RSS growth {median - info['baseline']} KiB")
        degree = sum(node in edge for edge in self.topology.edges)
        if fds > 64 + 8 * degree:
            raise RuntimeError(f"node {node}: FD count {fds} exceeds topology-sized budget")
        extra = {}
        if self.implementations[node] == "babel-rs":
            limits = status["limits"]
            if status["candidates"] > limits["max_candidates"] or status["neighbors"] > limits["max_neighbors"]:
                raise RuntimeError(f"node {node}: learned-state limit exceeded")
            if any(status[key] for key in ("rejected_neighbors", "rejected_candidates_global", "rejected_candidates_per_neighbor")):
                raise RuntimeError(f"node {node}: topology exceeded admission capacity")
            extra = {key: status[key] for key in ("candidates", "sources", "pending_requests", "route_generation", "export")}
        self.samples.info(json.dumps({"round": self.round, "node": node, "pid": proc.pid,
                                      "implementation": self.implementations[node],
                                      "elapsed": round(time.monotonic() - self.started, 3),
                                      "rss_kib": rss, "fds": fds, "baseline_kib": info["baseline"],
                                      **extra}))

    def observe(self):
        observations = {}
        self.latest = observations
        for node in sorted(self.nodes):
            info = self.nodes[node]
            if info["proc"].poll() is not None:
                raise RuntimeError(f"node {node}: unexpected process exit {info['proc'].returncode}")
            try:
                status = self.command(node, "status")
            except (FileNotFoundError, ConnectionRefusedError):
                if info["ready"]:
                    raise RuntimeError(f"node {node}: established control socket disappeared") from None
                raise NotConverged(f"node {node}: waiting for initial control socket") from None
            if not info["ready"]:
                info["ready"] = True
                self.record("node-ready", node=node, pid=info["proc"].pid,
                            implementation=self.implementations[node],
                            sequence_number=status["sequence_number"] if isinstance(status, dict) else None)
            self.resources(node, status)
            observations[node] = {"implementation": self.implementations[node], "status": status,
                                  "routes": self.command(node, "routes") if self.implementations[node] == "babel-rs" else None,
                                  "fib": json.loads(self.run("ip", "-n", self.namespace(node), "-6", "-j",
                                                             "route", "show", "table", str(TABLE)).stdout)}
        return observations

    def ping(self, pair):
        source, destination = pair
        result = self.run("ip", "netns", "exec", self.namespace(source), "ping", "-6", "-n", "-c", "1",
                          "-W", "1", "-I", address(source), address(destination), check=False)
        if result.returncode == 2 and any(message in result.stderr for message in
                                         ("Network is unreachable", "No route to host")):
            return False
        if result.returncode not in (0, 1):
            raise RuntimeError(f"ping tool failure {pair}: {result.stdout} {result.stderr}")
        return result.returncode == 0

    def probes(self, pairs):
        if pairs:
            count = min(len(pairs), self.args.probe_pairs)
            selected = [pairs[(self.probe_cursor + index) % len(pairs)] for index in range(count)]
            self.probe_cursor += count
            for pair in selected:
                if not self.ping(pair):
                    raise NotConverged(f"data-plane probe failed: {pair}")
        components = self.topology.components()
        disconnected = [(node, peer) for node in sorted(self.topology.active) for peer in range(self.topology.size)
                        if peer not in components[node]]
        if disconnected:
            pair = disconnected[self.round % len(disconnected)]
            if self.ping(pair):
                raise NotConverged(f"unexpected reachability: {pair}")

    def save(self, name, value):
        temporary = self.artifacts / (name + ".tmp")
        temporary.write_text(json.dumps(value, indent=2))
        temporary.replace(self.artifacts / name)

    def verify(self, unaffected):
        start, good_since, last_report, reason = time.monotonic(), None, 0, "waiting for first audit"
        checkpoints = {}
        self.verification = {"round": self.round, "attempts": 0, "resets": 0, "last_rejection": None}
        self.deadline = start + self.args.settle_timeout
        while time.monotonic() < self.deadline:
            # An unchanged connected component has no permitted convergence gap.
            for pair in unaffected[:2]:
                if not self.ping(pair):
                    raise RuntimeError(f"unaffected component lost forwarding: {pair}")
            try:
                self.verification["attempts"] += 1
                stage = "observe"
                observations = self.observe()
                stage = "rib-fib"
                pairs = audit(self.topology, observations)
                stage = "export"
                exports_confirmed = True
                for node, observation in observations.items():
                    if observation.get("implementation", "babel-rs") != "babel-rs":
                        continue  # Foreign implementations are audited via the kernel/data plane.
                    status, export = observation["status"], observation["status"]["export"]
                    target = checkpoints.setdefault(node, {"route": status["route_generation"],
                                                           "config": export["config_generation"],
                                                           "started": time.monotonic()})
                    confirmed = (export["last_success_route_generation"] is not None
                                 and export["last_success_route_generation"] >= target["route"]
                                 and export["last_success_config_generation"] is not None
                                 and export["last_success_config_generation"] >= target["config"])
                    if (export["last_error"] is not None
                            or export["last_success_age_seconds"] is None
                            or export["last_success_age_seconds"] >= 10
                            or (not confirmed and time.monotonic() - target["started"] >= 10)):
                        raise NotConverged(f"node {node}: export has not caught up; "
                                           f"target_route={target['route']} target_config={target['config']} "
                                           f"route={status['route_generation']} "
                                           f"ack={export['last_success_route_generation']} "
                                           f"config={export['config_generation']} "
                                           f"ack_config={export['last_success_config_generation']} "
                                           f"age={export['last_success_age_seconds']} "
                                           f"error={export['last_error']!r}")
                    exports_confirmed &= confirmed
                self.verification["export_checkpoints"] = checkpoints.copy()
                stage = "probes"
                self.probes(pairs)
                if good_since is None:
                    good_since = time.monotonic()
                    self.record("stable-start", attempt=self.verification["attempts"])
                reason = "verifying stable window"
                if exports_confirmed and time.monotonic() - good_since >= self.args.stable_seconds:
                    self.record("verified", seconds=round(time.monotonic() - start, 3),
                                active_nodes=len(self.nodes), active_edges=len(self.topology.live_edges()),
                                implementations={kind: sum(self.implementations[node] == kind for node in self.nodes)
                                                 for kind in sorted(set(self.implementations.values()))},
                                actual_avg_degree=2 * len(self.topology.live_edges()) / len(self.nodes),
                                reachable_pairs=len(pairs), operations=self.counts)
                    self.save("latest.json", {"round": self.round, "topology": self.topology.state(),
                                              "observations": self.latest})
                    self.deadline = None
                    if self.round == 0:
                        self.initial_verified = True
                    return
            except NotConverged as error:
                rejection = {"stage": stage, "reason": str(error),
                             "attempt": self.verification["attempts"],
                             "stable_seconds": round(time.monotonic() - good_since, 3) if good_since is not None else 0}
                self.verification["last_rejection"] = rejection
                self.record("audit-retry", **rejection)
                if good_since is not None:
                    self.verification["resets"] += 1
                    self.save("last-reset.json", {"round": self.round, "elapsed": round(time.monotonic() - self.started, 3),
                                                  "rejection": rejection, "topology": self.topology.state(),
                                                  "observations": self.latest})
                good_since, reason = None, str(error)
                checkpoints = {}
            if time.monotonic() - last_report >= 30:
                self.record("waiting", reason=reason)
                last_report = time.monotonic()
            time.sleep(min(2, self.timeout(2)))
        raise TimeoutError(f"round {self.round}: {reason}; last rejection: {self.verification['last_rejection']}")

    def execute(self):
        self.deadline = time.monotonic() + self.args.settle_timeout
        for node in sorted(self.topology.active):
            self.add_node(node)
        self.verify([])
        while self.args.rounds == 0 or self.round < self.args.rounds:
            self.round += 1
            components = self.topology.components()
            touched = set()
            self.deadline = time.monotonic() + self.args.settle_timeout
            for _ in range(self.args.changes):
                event = self.topology.next_event()
                touched.update(self.topology.touched(event))
                self.apply(event)
            unaffected = [(node, peer) for node in sorted(components)
                          if not (components[node] & touched)
                          for peer in sorted(components[node] - {node})]
            self.verify(unaffected)
        self.record("complete", result="PASS", operations=self.counts)

    def snapshot_failure(self, error):
        self.deadline = None
        self.record("stopped", reason=str(error))
        self.save("failure.json", {"round": self.round, "error": str(error), "topology": self.topology.state(),
                                   "observations": self.latest, "operations": self.counts,
                                   "verification": self.verification})
        self.deadline = time.monotonic() + 30
        for node in sorted(self.created):
            if time.monotonic() >= self.deadline:
                self.record("diagnostics-truncated", reason="30-second budget")
                break
            diagnostics = {"implementation": self.implementations[node]}
            for operation in ("status", "interfaces", "neighbors", "routes"):
                try:
                    diagnostics[operation] = self.command(node, operation)
                except Exception as failure:
                    diagnostics[operation] = {"error": str(failure)}
            for name, args in (("addresses", ("-6", "addr", "show")),
                               ("routes", ("-6", "route", "show", "table", "all"))):
                try:
                    diagnostics["kernel_" + name] = self.run("ip", "-n", self.namespace(node), *args, check=False).stdout
                except Exception as failure:
                    diagnostics["kernel_" + name] = str(failure)
            self.save(f"diagnostic-{node}.json", diagnostics)
        self.deadline = None
        for source in self.runtime.iterdir():
            if source.suffix in (".toml", ".state", ".conf"):
                shutil.copyfile(source, self.artifacts / source.name)

    def cleanup(self):
        self.deadline = None
        failures = []
        terminated = set()
        for node, info in self.nodes.items():
            if info["proc"].poll() is None:
                info["proc"].terminate()
                terminated.add(node)
        stop_by = time.monotonic() + 8
        for node, info in self.nodes.items():
            proc = info["proc"]
            try:
                proc.wait(timeout=max(0.01, stop_by - time.monotonic()))
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=3)
                failures.append(f"node {node}: forced cleanup kill")
            if proc.returncode and node in terminated:
                failures.append(f"node {node}: cleanup exit {proc.returncode}")
            info["thread"].join(timeout=3)
            if info["thread"].is_alive():
                failures.append(f"node {node}: log drain still alive")
        for node in sorted(self.created):
            try:
                result = self.run("ip", "netns", "del", self.namespace(node), check=False)
                if result.returncode:
                    failures.append(f"namespace {node}: {result.stderr}")
            except Exception as error:
                failures.append(f"namespace {node}: {error}")
        for device in self.temporary_links:
            try:
                # Removing a namespace or one veth endpoint also removes its
                # peer. Only delete tracked temporary devices still present.
                present = json.loads(self.run("ip", "-j", "link", "show").stdout)
                if not any(link["ifname"] == device for link in present):
                    continue
                result = self.run("ip", "link", "del", device, check=False)
                if result.returncode:
                    failures.append(f"link {device}: {result.stderr}")
            except Exception as error:
                failures.append(f"link {device}: {error}")
        self.record("cleanup", result="FAIL" if failures else "PASS", errors=failures)
        for logger in (self.events, self.samples, *self.node_logs.values()):
            for handler in logger.handlers:
                handler.close()
        return failures


def main():
    args, topology = arguments()
    if args.plan:
        print(json.dumps({"graph": args.graph, "nodes": args.nodes, "min_nodes": args.min_nodes,
                          "seed": args.seed, "edges": topology.edges,
                          "implementations": assign_instances(args.nodes, args.seed, args.mix)}))
        for round_number in range(1, args.rounds + 1):
            events = [topology.next_event() for _ in range(args.changes)]
            print(json.dumps({"round": round_number, "events": events, "state": topology.state()}))
        return 0
    if args.validate:
        return 0
    if os.geteuid() != 0:
        raise SystemExit("run on a disposable root Linux test host")
    for binary in ("ip", "ping", "sysctl"):
        if shutil.which(binary) is None:
            raise SystemExit(f"missing tool: {binary}")
    for kind, weight in parse_mix(args.mix).items():
        if weight and kind != "babel-rs" and shutil.which(str(getattr(args, kind))) is None:
            raise SystemExit(f"missing {kind} executable: {getattr(args, kind)}")
    artifacts = (args.artifacts or Path(".local/experiments/endless") / f"{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}").resolve()
    if args.on_failure == "next-seed":
        from campaign import run_campaign
        return run_campaign(args, artifacts)
    artifacts.mkdir(parents=True, exist_ok=False)
    with tempfile.TemporaryDirectory(prefix="babel-endless-") as directory:
        runner = Runner(args, topology, Path(directory), artifacts)
        with args.daemon.open("rb") as binary:
            binary_hash = hashlib.file_digest(binary, "sha256").hexdigest()
        binaries = {}
        for kind in sorted(set(runner.implementations.values())):
            executable = Path(runner.binaries[kind]).resolve()
            with executable.open("rb") as binary:
                digest = hashlib.file_digest(binary, "sha256").hexdigest()
            version = runner.run(str(executable), "-V" if kind == "babeld" else "--version", check=False)
            binaries[kind] = {"path": str(executable), "sha256": digest,
                              "version": (version.stdout + version.stderr).strip()}
        runner.save("manifest.json", {"arguments": {key: str(value) if isinstance(value, Path) else value
                                                   for key, value in vars(args).items()},
                                      "binary_sha256": binary_hash, "kernel": os.uname().release,
                                      "pid": os.getpid(), "namespace_prefix": f"vbe-{runner.token}-",
                                      "python": os.sys.version, "edges": topology.edges,
                                      "routing_table": TABLE,
                                      "implementations": runner.implementations,
                                      "binaries": binaries,
                                      "origins": {node: prefix(node) for node in range(args.nodes)}})

        for name in ("netns.py", "model.py", "instances.py"):
            shutil.copyfile(Path(__file__).with_name(name), artifacts / name)

        def stop(_signum, _frame):
            raise StopRequested("operator requested stop")

        signal.signal(signal.SIGINT, stop)
        signal.signal(signal.SIGTERM, stop)
        signal.signal(signal.SIGHUP, stop)
        code = 0
        try:
            runner.record("start", artifacts=str(artifacts), graph=args.graph, seed=args.seed)
            runner.execute()
        except StopRequested as error:
            code = 130
            runner.snapshot_failure(error)
        except Exception as error:
            code = 1
            runner.snapshot_failure(error)
        finally:
            signal.signal(signal.SIGINT, signal.SIG_IGN)
            signal.signal(signal.SIGTERM, signal.SIG_IGN)
            signal.signal(signal.SIGHUP, signal.SIG_IGN)
            cleanup_errors = runner.cleanup()
            if cleanup_errors:
                code = 1
        runner.save("outcome.json", {"exit_code": code, "round": runner.round,
                                     "initial_verified": runner.initial_verified,
                                     "cleanup_ok": not cleanup_errors})
        return code


if __name__ == "__main__":
    raise SystemExit(main())
