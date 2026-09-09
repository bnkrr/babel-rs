"""Manual oracle checks: python3 -m unittest discover -s tests/endless."""
import copy
import unittest

from model import NotConverged, Topology, audit, link_local, prefix


def observations(topology):
    graph = topology.adjacency()
    result = {}
    for source in sorted(graph):
        first, pending = {}, [(source, None)]
        visited = {source}
        for node, next_hop in pending:
            for peer in sorted(graph[node] - visited):
                visited.add(peer)
                first[peer] = next_hop if next_hop is not None else peer
                pending.append((peer, first[peer]))
        result[source] = {"routes": {"routes": []}, "fib": []}
        for destination, peer in sorted(first.items()):
            set_route(result, topology, source, destination, peer)
    return result


def set_route(result, topology, node, destination, peer):
    interface = f"e{topology.edges.index(tuple(sorted((node, peer))))}"
    rib = {"destination": prefix(destination), "source": None, "metric": 192,
           "interface": interface, "next_hop": link_local(peer)}
    fib = {"dst": prefix(destination), "dev": interface, "gateway": link_local(peer)}
    result[node]["routes"]["routes"] = [r for r in result[node]["routes"]["routes"] if r["destination"] != prefix(destination)] + [rib]
    result[node]["fib"] = [r for r in result[node]["fib"] if r["dst"] != prefix(destination)] + [fib]


class TopologyTests(unittest.TestCase):
    def test_graph_shapes_density_and_connectivity(self):
        for kind in ("mesh", "bottleneck", "hub"):
            for seed in range(20):
                model = Topology(8, 3, kind, seed)
                self.assertEqual(len(model.edges), 12)
                self.assertEqual(len(model.components()[0]), 8)
                self.assertTrue(all(a < b for a, b in model.edges))
                if kind == "bottleneck":
                    bridges = []
                    for index in range(len(model.edges)):
                        model.enabled.remove(index)
                        if len(model.components()[0]) != 8:
                            bridges.append(index)
                        model.enabled.add(index)
                    self.assertTrue(bridges)
                if kind == "hub":
                    self.assertEqual(max(map(len, model.adjacency().values())), 7)

    def test_impossible_density_and_nonfinite_inputs_are_rejected(self):
        for count, degree, kind in ((2, 2, "mesh"), (65, 2, "mesh"), (8, 1, "mesh"),
                                    (8, 4, "bottleneck"), (8, float("nan"), "mesh"),
                                    (8, float("inf"), "hub")):
            with self.assertRaises(ValueError):
                Topology(count, degree, kind, 1)

    def test_plan_is_repeatable_and_bounded_under_long_churn(self):
        left, right = Topology(8, 3, "mesh", 9), Topology(8, 3, "mesh", 9)
        operations = set()
        for _ in range(5000):
            event = left.next_event()
            self.assertEqual(event, right.next_event())
            self.assertEqual(left.state(), right.state())
            self.assertTrue(2 <= len(left.active) <= 8)
            self.assertTrue(all(set(edge) <= left.active for edge in left.live_edges()))
            operations.add(event["operation"])
        self.assertEqual(operations, {"add-node", "delete-node", "link-up", "link-down"})

    def test_configured_minimum_under_long_churn(self):
        model = Topology(32, 4, "bottleneck", 20260908, min_nodes=16)
        counts = {len(model.active)}
        operations = set()
        for _ in range(5000):
            operations.add(model.next_event()["operation"])
            counts.add(len(model.active))
            self.assertTrue(16 <= len(model.active) <= 32)
        self.assertEqual(min(counts), 16)
        self.assertEqual(operations, {"add-node", "delete-node", "link-up", "link-down"})

    def test_minimum_equal_to_pool_only_changes_links(self):
        model = Topology(3, 2, "mesh", 1, min_nodes=3)
        operations = {model.next_event()["operation"] for _ in range(100)}
        self.assertEqual(operations, {"link-up", "link-down"})
        self.assertEqual(len(model.active), 3)

    def test_invalid_minimum_is_rejected(self):
        for minimum in (-1, 0, 1, 9):
            with self.assertRaisesRegex(ValueError, "min_nodes"):
                Topology(8, 3, "mesh", 1, min_nodes=minimum)


class OracleTests(unittest.TestCase):
    def setUp(self):
        self.model = Topology(3, 2, "mesh", 1)
        self.observed = observations(self.model)

    def test_connected_and_partitioned_oracle(self):
        self.assertEqual(len(audit(self.model, self.observed)), 6)
        self.model.enabled = {0}
        self.assertEqual(len(audit(self.model, observations(self.model))), 2)

    def test_nonshortest_acyclic_path_is_valid(self):
        set_route(self.observed, self.model, 0, 2, 1)
        self.assertEqual(len(audit(self.model, self.observed)), 6)

    def test_cycle_is_rejected_even_when_rib_and_fib_agree(self):
        set_route(self.observed, self.model, 0, 2, 1)
        set_route(self.observed, self.model, 1, 2, 0)
        with self.assertRaisesRegex(NotConverged, "cycle"):
            audit(self.model, self.observed)

    def test_missing_rib_missing_fib_and_wrong_gateway_are_rejected(self):
        for fault in ("rib", "fib", "gateway", "duplicate"):
            sample = copy.deepcopy(self.observed)
            if fault == "rib":
                sample[0]["routes"]["routes"].pop()
            elif fault == "fib":
                sample[0]["fib"].pop()
            elif fault == "gateway":
                sample[0]["fib"][0]["gateway"] = "fe80::ffff"
            else:
                sample[0]["fib"].append(copy.deepcopy(sample[0]["fib"][0]))
            with self.assertRaises(NotConverged, msg=fault):
                audit(self.model, sample)

    def test_stale_routes_to_removed_node_are_rejected(self):
        self.model.active.remove(2)
        with self.assertRaises(NotConverged):
            audit(self.model, self.observed)
        correct = observations(self.model)
        correct[0]["fib"].append({"dst": prefix(2), "type": "unreachable"})
        self.assertEqual(len(audit(self.model, correct)), 2)

    def test_forwarding_on_down_link_is_rejected(self):
        self.model.enabled.remove(self.model.edges.index((0, 2)))
        with self.assertRaisesRegex(NotConverged, "down link"):
            audit(self.model, self.observed)


if __name__ == "__main__":
    unittest.main()
