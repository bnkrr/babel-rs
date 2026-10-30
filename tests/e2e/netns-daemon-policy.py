#!/usr/bin/env python3
"""Daemon import/announcement/install policies over a dual-stack three-node path."""
import importlib.util
import json
from pathlib import Path
import sys

spec = importlib.util.spec_from_file_location("rfc_lab", Path(__file__).with_name("netns-rfc-boundaries.py"))
rfc = importlib.util.module_from_spec(spec)
spec.loader.exec_module(rfc)


def main(daemon):
    with rfc.Lab(daemon, "daemon-policy", 3) as lab:
        lab.link(0, 1, right="lan0")
        lab.link(1, 2, left="lan1")
        destinations = [(4, "198.51.100.1/32", "0.0.0.0/0", "192.0.2.0/24"),
                        (6, "2001:db8::1/128", "::/0", "2001:db8:ffff::/64")]
        bases = {}
        for node in range(3):
            lab.config(node, origin=False)
            path = lab.root / f"{node}.toml"
            text = path.read_text()
            if node == 0:
                origins = ""
                for _, destination, zero, source in destinations:
                    origins += f'[[origins]]\ndestination = "{destination}"\n'
                    origins += f'[[origins]]\ndestination = "{zero}"\n'
                    origins += f'[[origins]]\ndestination = "{zero}"\nsource = "{source}"\n'
                text = text.replace("[export]", origins + "[export]")
            for table, (_, _, _, source) in zip((202, 203), destinations):
                text += f'[[export.views]]\ntable = {table}\nsource = "{source}"\n'
            bases[node] = text
            path.write_text(text)

        def configure(node, policy="", install=""):
            text = bases[node] + policy
            if node == 1:
                text += '[[export.views]]\ntable = 211\n' + install
            (lab.root / f"{node}.toml").write_text(text)
            if len(lab.processes) == 3:
                lab.control(node, "reload")

        def routes(node, family, table, destination):
            result = lab.ip(node, f"-{family}", "-j", "route", "show", "table", str(table), "exact", destination, check=False)
            return json.loads(result.stdout) if result.returncode == 0 else []

        def finite(node, family, destination, table=201):
            return any(r.get("type", "unicast") == "unicast" for r in routes(node, family, table, destination))

        def selected(node, destination, source):
            return any(r["destination"] == destination and r["source"] == source
                       for r in lab.control(node, "routes")["routes"])

        def deny(stage, ordinary=False, source_only=False):
            result = ""
            for _, destination, zero, source in destinations:
                result += f'[[policy.{stage}.rules]]\naction = "deny"\n'
                if stage == "announce":
                    result += 'interfaces = ["lan1"]\n'
                if ordinary:
                    result += f'source = {{ prefix = "{zero}", match = "exact" }}\n'
                    destination = zero
                if source_only:
                    result += f'source = {{ prefix = "{source}", match = "exact" }}\n'
                else:
                    result += f'destination = {{ prefix = "{destination}", match = "exact" }}\n'
            return result

        for node in range(3):
            configure(node)
            lab.start(node)
        lab.wait("daemon-policy-default-complete", lambda: all(
            finite(node, family, destination) and finite(node, family, zero, table)
            for node in (1, 2) for table, (family, destination, zero, _) in zip((202, 203), destinations)))

        configure(1, policy=deny("import", ordinary=True))
        lab.wait("ordinary-default-filter-keeps-sadr", lambda: all(
            not finite(node, family, zero) and finite(node, family, zero, table)
            for node in (1, 2) for table, (family, _, zero, _) in zip((202, 203), destinations)))
        configure(1, policy=deny("import", source_only=True))
        lab.wait("source-prefix-filter-keeps-ordinary", lambda: all(
            finite(node, family, zero) and not selected(node, zero, source)
            for node in (1, 2) for family, _, zero, source in destinations))
        configure(1)
        lab.wait("import-relaxation-relearns", lambda: all(
            finite(node, family, zero, table)
            for node in (1, 2) for table, (family, _, zero, _) in zip((202, 203), destinations)))

        configure(1, policy=deny("announce"))
        lab.wait("announcement-filter-keeps-local-routes", lambda: all(
            finite(1, family, destination) and not finite(2, family, destination)
            for family, destination, _, _ in destinations))
        configure(1)
        lab.wait("announcement-relaxation", lambda: all(finite(2, f, d) for f, d, _, _ in destinations))

        configure(1, install='[export.views.filter]\ndefault = "deny"\n')
        lab.wait("table-filter-independent-of-protocol", lambda: all(
            finite(1, f, d) and finite(2, f, d) and not finite(1, f, d, 211)
            for f, d, _, _ in destinations))
        # Invalid rules must fail before changing the active generation.
        generation = lab.control(1, "status")["config_generation"]
        (lab.root / "1.toml").write_text(bases[1] + '[[policy.import.rules]]\naction = "allow"\nroute_kind = "ordinary"\n')
        try:
            lab.control(1, "reload")
        except AssertionError:
            pass
        else:
            raise AssertionError("invalid route rule was accepted")
        assert lab.control(1, "status")["config_generation"] == generation
        configure(1, install='[export.views.filter]\ndefault = "deny"\n')

        for _, destination, _, _ in destinations:
            bases[0] = bases[0].replace(f'[[origins]]\ndestination = "{destination}"\n', "")
        configure(0)
        lab.wait("withdrawal-holds-bypass-install-filter", lambda: all(
            any(r.get("type") == "unreachable" for r in routes(1, f, table, d))
            for f, d, _, _ in destinations for table in (201, 211)))
        print("daemon policy E2E passed", flush=True)


if __name__ == "__main__":
    main(str(Path(sys.argv[1]).resolve()))
