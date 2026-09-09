"""Topology and independent route oracle for the opt-in endless test."""
import ipaddress
import math
import random


class NotConverged(Exception):
    """A network observation does not yet match the desired topology."""


def prefix(node):
    return f"2001:db8:{node + 1:x}::1/128"


def address(node):
    return prefix(node).split("/")[0]


def link_local(node):
    return f"fe80::{node + 1:x}"


def network(value):
    return str(ipaddress.ip_network(value, strict=False))


class Topology:
    def __init__(self, nodes, average_degree, kind, seed, min_nodes=2):
        if not 3 <= nodes <= 64:
            raise ValueError("nodes must be in 3..64")
        if not 2 <= min_nodes <= nodes:
            raise ValueError("min_nodes must be in 2..nodes")
        if kind not in ("mesh", "bottleneck", "hub"):
            raise ValueError("unknown graph type")
        graph_rng = random.Random(seed)
        self.rng = random.Random(seed ^ 0xBABE1)
        self.size = nodes
        self.min_nodes = min_nodes
        self.active = set(range(nodes))
        groups = [list(range(nodes))]
        if kind == "bottleneck":
            order = list(range(nodes))
            graph_rng.shuffle(order)
            groups = [order[:nodes // 2], order[nodes // 2:]]
        allowed = {tuple(sorted((a, b))) for group in groups for a in group for b in group if a < b}
        edges = set()
        for group in groups:
            order = group.copy()
            graph_rng.shuffle(order)
            for index, node in enumerate(order[1:], 1):
                peer = order[0] if kind == "hub" else graph_rng.choice(order[:index])
                edges.add(tuple(sorted((node, peer))))
        if kind == "bottleneck":
            bridge = tuple(sorted((graph_rng.choice(groups[0]), graph_rng.choice(groups[1]))))
            allowed.add(bridge)
            edges.add(bridge)
        minimum, maximum = 2 * len(edges) / nodes, 2 * len(allowed) / nodes
        if average_degree is None:
            average_degree = min(3.0, maximum)
        if not math.isfinite(average_degree) or not minimum <= average_degree <= maximum:
            raise ValueError(f"average degree for {kind}/{nodes} must be in {minimum:g}..{maximum:g}")
        budget = math.floor(average_degree * nodes / 2 + 0.5)
        edges.update(graph_rng.sample(sorted(allowed - edges), budget - len(edges)))
        self.edges = tuple(sorted(edges))
        self.enabled = set(range(len(self.edges)))

    def live_edges(self):
        return {edge for index, edge in enumerate(self.edges)
                if index in self.enabled and set(edge) <= self.active}

    def adjacency(self):
        result = {node: set() for node in self.active}
        for a, b in self.live_edges():
            result[a].add(b)
            result[b].add(a)
        return result

    def components(self):
        graph = self.adjacency()
        result = {}
        for node in sorted(graph):
            if node in result:
                continue
            seen, pending = set(), [node]
            while pending:
                current = pending.pop()
                if current not in seen:
                    seen.add(current)
                    pending.extend(graph[current] - seen)
            for member in seen:
                result[member] = seen
        return result

    def state(self):
        return {"active": sorted(self.active), "enabled_edges": sorted(self.enabled),
                "live_edges": sorted(self.live_edges())}

    def next_event(self):
        choices = {}
        if len(self.active) > self.min_nodes:
            choices["delete-node"] = sorted(self.active)
        absent = sorted(set(range(self.size)) - self.active)
        if absent:
            choices["add-node"] = absent
        for operation, enabled in (("link-down", True), ("link-up", False)):
            candidates = [index for index, edge in enumerate(self.edges)
                          if set(edge) <= self.active and (index in self.enabled) == enabled]
            if candidates:
                choices[operation] = candidates
        operation = self.rng.choice(sorted(choices))
        target = self.rng.choice(choices[operation])
        if operation == "add-node":
            self.active.add(target)
        elif operation == "delete-node":
            self.active.remove(target)
        elif operation == "link-up":
            self.enabled.add(target)
        else:
            self.enabled.remove(target)
        return {"operation": operation, "target": target}

    def touched(self, event):
        if event["operation"].endswith("node"):
            return {event["target"]}
        return set(self.edges[event["target"]])


def audit(topology, observations):
    """Require graph reachability, coherent RIB/FIB and acyclic FIB forwarding.

    Input is raw control routes (babel-rs only) plus `ip -6 -j route show table ...`. Graph
    connectivity, origin ownership and next-hop identities come from the host.
    Neither advertised metrics nor another babel-rs instance acts as the oracle.
    Returns all ordered reachable pairs for independent data-plane probes.
    """
    components = topology.components()
    live_edges = topology.live_edges()
    forwarding = {}
    for node in sorted(topology.active):
        expected = {prefix(peer) for peer in components[node] - {node}}
        observation = observations[node]
        kind = observation.get("implementation", "babel-rs")
        if kind not in ("babel-rs", "bird", "babeld"):
            raise ValueError(f"unknown implementation {kind}")
        rib = None
        if kind == "babel-rs":
            rib = observation["routes"]["routes"]
            if len(rib) != len({route["destination"] for route in rib}):
                raise NotConverged(f"node {node}: duplicate selected routes")
            if {route["destination"] for route in rib} != expected:
                raise NotConverged(f"node {node}: selected RIB differs from graph reachability")
        fib = {}
        for route in observations[node]["fib"]:
            if route.get("from") not in (None, "all", "::/0"):
                raise NotConverged(f"node {node}: unexpected source-specific FIB route")
            if route.get("type", "unicast") != "unicast":
                if (route.get("type") != "unreachable"
                        or network(route["dst"]) not in {prefix(peer) for peer in range(topology.size)} - expected - {prefix(node)}):
                    raise NotConverged(f"node {node}: unexpected non-unicast FIB route {route}")
                continue  # RFC unreachable hold routes to lost destinations.
            key = network(route["dst"])
            if key in fib:
                raise NotConverged(f"node {node}: duplicate active FIB route {key}")
            fib[key] = route
        if set(fib) != expected:
            raise NotConverged(f"node {node}: active FIB differs from graph reachability; "
                               f"missing={sorted(expected - fib.keys())} extra={sorted(fib.keys() - expected)}")
        forwarding[node] = {}
        for key, kernel in fib.items():
            interface = kernel.get("dev", "")
            try:
                index = int(interface.removeprefix("e"))
                if not 0 <= index < len(topology.edges):
                    raise IndexError(index)
                edge = topology.edges[index]
            except (ValueError, IndexError):
                raise NotConverged(f"node {node}: unknown interface {interface}") from None
            if interface != f"e{index}" or node not in edge or edge not in live_edges:
                raise NotConverged(f"node {node}: next hop uses a missing/down link")
            peer = edge[1] if edge[0] == node else edge[0]
            if kernel.get("gateway") != link_local(peer) or kernel.get("multipath") or kernel.get("nexthops"):
                raise NotConverged(f"node {node}: invalid FIB next hop for {key}")
            forwarding[node][key] = peer
        for route in rib or []:
            key = route["destination"]
            if route.get("source") is not None or not 0 < route["metric"] < 65535:
                raise NotConverged(f"node {node}: invalid selected route {key}")
            kernel = fib[key]
            if route["interface"] != kernel.get("dev") or route["next_hop"] != kernel.get("gateway"):
                raise NotConverged(f"node {node}: RIB/FIB next hop mismatch for {key}")
    pairs = []
    for source in sorted(topology.active):
        for destination in sorted(components[source] - {source}):
            seen, node = set(), source
            while node != destination:
                if node in seen:
                    raise NotConverged(f"FIB cycle {source}->{destination} through {sorted(seen)}")
                seen.add(node)
                node = forwarding[node][prefix(destination)]
            pairs.append((source, destination))
    return pairs
