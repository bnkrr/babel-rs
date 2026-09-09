"""Daemon-specific configuration and read-only control for the endless fixture."""
import random
import socket
import time

from model import prefix


KINDS = ("babel-rs", "bird", "babeld")


def parse_mix(value):
    weights = {}
    try:
        for entry in value.split(","):
            name, weight = entry.split("=")
            name, weight = name.strip(), int(weight)
            if name not in KINDS or name in weights or not 0 <= weight <= 1_000_000:
                raise ValueError
            weights[name] = weight
        if not any(weights.values()):
            raise ValueError
    except ValueError:
        raise ValueError("mix must contain unique babel-rs/bird/babeld integer weights, "
                         "0..1000000, with a positive total (e.g. babel-rs=2,bird=1,babeld=1)") from None
    return {kind: weights.get(kind, 0) for kind in KINDS}


def assign_instances(nodes, seed, mix):
    # Never consume the topology/event RNG. A slot keeps its implementation on
    # recreation; changing only the mix preserves the original topology plan.
    weights = parse_mix(mix)
    rng = random.Random(seed ^ 0x1A57A)
    return dict(enumerate(rng.choices(KINDS, weights=list(weights.values()), k=nodes)))


def launch(kind, binary, node, runtime, edges, table, protocol):
    """Write one instance's config; return a foreground argv (without netns)."""
    control = runtime / f"{node}.ctl"
    control.unlink(missing_ok=True)
    # The controller has waited for the previous process before recreating a
    # slot. babeld uses O_EXCL for its PID file, which survives a SIGKILL.
    (runtime / f"{node}.pid").unlink(missing_ok=True)
    state = runtime / f"{node}.state"
    if kind == "babel-rs":
        config = runtime / f"{node}.toml"
        config.write_text(f'''router_id = "{node + 1:016x}"
state_file = "{state}"
[[interfaces]]
match = ["e*"]
[[origins]]
destination = "{prefix(node)}"
[export]
protocol = {protocol}
manage_rules = false
[[export.views]]
table = {table}
''')
        return [binary, "run", "--config", str(config), "--control-socket", str(control)]
    config = runtime / f"{node}.conf"
    if kind == "bird":
        config.write_text(f'''log stderr all;
router id 0.0.0.{node + 1};
ipv6 table test6;
protocol device {{ scan time 1; }}
protocol static origin {{
  ipv6 {{ table test6; }};
  route {prefix(node)} blackhole;
}}
protocol babel mesh {{
  randomize router id no;
  ipv6 {{ table test6; import all; export all; }};
  interface "e*" {{ type wired; hello interval 4 s; update interval 16 s; }};
}}
protocol kernel fib {{
  ipv6 {{ table test6; import none; export filter {{
    if proto = "mesh" && net != {prefix(node)} then accept;
    reject;
  }}; }};
  kernel table {table};
  scan time 1;
}}
''')
        return [binary, "-f", "-c", str(config), "-s", str(control),
                "-P", str(runtime / f"{node}.pid")]
    if kind == "babeld":
        router_id = ":".join(f"{byte:02x}" for byte in (node + 1).to_bytes(8, "big"))
        interfaces = [f"e{index}" for index, edge in enumerate(edges) if node in edge]
        config.write_text(f'''router-id {router_id}
local-path {control}
default type wired hello-interval 4 update-interval 16
redistribute ip {prefix(node)} eq 128 allow
redistribute deny
''' + "".join(f"interface {name}\n" for name in interfaces))
        # Config names include absent potential interfaces: babeld discovers
        # them when peers are added, without restarting surviving instances.
        return [binary, "-c", str(config), "-S", str(state), "-t", str(table),
                "-I", str(runtime / f"{node}.pid")]
    raise ValueError(f"unknown implementation {kind}")


def foreign_command(kind, path, command, timeout):
    """Bounded local Unix control reply. No TCP listeners or write commands."""
    deadline = time.monotonic() + timeout
    with socket.socket(socket.AF_UNIX) as sock:
        sock.settimeout(timeout)
        sock.connect(str(path))
        with sock.makefile("rwb") as stream:
            def reply():
                lines, size = [], 0
                while True:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        raise TimeoutError("control reply deadline exceeded")
                    sock.settimeout(remaining)
                    line = stream.readline(1024 * 1024 + 1)
                    size += len(line)
                    if not line or size > 1024 * 1024:
                        raise RuntimeError("closed or oversized control reply")
                    text = line.decode(errors="replace").rstrip("\r\n")
                    lines.append(text)
                    if kind == "babeld":
                        if text == "ok":
                            return "\n".join(lines)
                        if text.startswith(("bad", "no")):
                            raise RuntimeError(f"babeld control error: {text}")
                    elif len(text) >= 4 and text[:4].isdigit() and (len(text) == 4 or text[4] == " "):
                        if int(text[:4]) >= 8000:
                            raise RuntimeError(f"BIRD control error: {text}")
                        return "\n".join(lines)
            greeting = reply()
            if (kind == "babeld" and not greeting.startswith("BABEL ")
                    or kind == "bird" and not greeting.startswith("0001 ")):
                raise RuntimeError(f"unexpected {kind} control greeting")
            stream.write((command + "\n").encode())
            stream.flush()
            return reply()
