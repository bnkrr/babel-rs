#!/usr/bin/env python3
"""Root-only real Unix control client deadlines and permit reuse test.

Keeps one healthy client and 63 idle/slow clients open at once. The slow clients
remain open locally until all 63 server permits have been demonstrably reused.
Uses the production 30-second deadline; no daemon fault-injection hooks.
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


CLIENT_TIMEOUT = 30
MAX_CLIENTS = 64
GROUP_SIZE = 21
LARGE_COMMAND = "x" * 900_000


def run(*args):
    result = subprocess.run(args, capture_output=True, text=True, timeout=10)
    if result.returncode:
        raise RuntimeError(f"{args}: {result.stdout}\n{result.stderr}")
    return result.stdout


class Client:
    def __init__(self, path):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(2)
        self.buffer = bytearray()
        try:
            self.sock.connect(str(path))
            greeting = self.line()
            assert greeting["type"] == "hello" and greeting["api_version"] == 1, greeting
        except BaseException:
            self.sock.close()
            raise

    def line(self):
        while b"\n" not in self.buffer:
            part = self.sock.recv(65536)
            if not part:
                raise EOFError("server closed before a complete frame")
            self.buffer.extend(part)
            assert len(self.buffer) <= 1024 * 1024, "oversized server response"
        line, _, rest = self.buffer.partition(b"\n")
        self.buffer = bytearray(rest)
        return json.loads(line)

    def send(self, command):
        self.sock.sendall((json.dumps({"api_version": 1, "id": 1,
                                      "command": command, "params": {}}) + "\n").encode())

    def request(self, command):
        self.send(command)
        response = self.line()
        assert response["ok"], response
        return response["result"]

    def drain_closed(self):
        total = len(self.buffer)
        self.buffer.clear()
        while True:
            try:
                part = self.sock.recv(65536)
            except ConnectionResetError:
                return total
            if not part:
                return total
            total += len(part)
            assert total <= 1024 * 1024, "unexpected amount of buffered output"

    def close(self):
        self.sock.close()


def main():
    def interrupted(_signum, _frame):
        raise KeyboardInterrupt("test interrupted")

    signal.signal(signal.SIGTERM, interrupted)
    daemon = str(Path(sys.argv[1]).resolve())
    assert os.geteuid() == 0 and os.access(daemon, os.X_OK)
    namespace = f"vb-clients-{os.getpid()}"
    clients = []
    process = None
    created = False
    with tempfile.TemporaryDirectory(prefix="babel-control-clients.") as directory:
        root = Path(directory)
        control = root / "daemon.ctl"
        config = root / "daemon.toml"
        logfile = root / "daemon.log"
        config.write_text(f'''router_id = "1122334455667788"
state_file = "{root}/state.toml"
[[interfaces]]
match = ["absent*"]
[export]
protocol = 207
manage_rules = false
[[export.views]]
table = 25207
''')

        def connect(retry=False):
            deadline = time.monotonic() + (5 if retry else 0)
            while True:
                assert process.poll() is None, f"daemon exited: {process.returncode}"
                try:
                    client = Client(control)
                    clients.append(client)
                    return client
                except (FileNotFoundError, ConnectionRefusedError, ConnectionResetError, EOFError):
                    if not retry or time.monotonic() >= deadline:
                        raise
                    time.sleep(0.05)

        def rejected():
            with socket.socket(socket.AF_UNIX) as extra:
                extra.settimeout(2)
                extra.connect(str(control))
                try:
                    assert extra.recv(1) == b"", "connection admitted beyond MAX_CLIENTS"
                except ConnectionResetError:
                    pass

        try:
            run("ip", "netns", "add", namespace)
            created = True
            run("ip", "-n", namespace, "link", "set", "lo", "up")
            with logfile.open("w") as log:
                process = subprocess.Popen(
                    ["ip", "netns", "exec", namespace, daemon, "run", "--config", str(config),
                     "--control-socket", str(control)],
                    env={**os.environ, "RUST_LOG": "warn"}, stdout=log, stderr=log)
            healthy = connect(retry=True)
            assert healthy.request("status")["ready"]
            started = time.monotonic()
            idle = [connect() for _ in range(GROUP_SIZE)]
            writers = [connect() for _ in range(GROUP_SIZE)]
            readers = [connect() for _ in range(GROUP_SIZE)]
            assert 1 + len(idle) + len(writers) + len(readers) == MAX_CLIENTS
            for client in writers:
                client.sock.sendall(b'{"api_version":')  # Never terminate the request frame.
            for client in readers:
                # A large unknown-command response fills the real Unix send
                # buffer. Do not drain it until after the server's write timeout.
                client.send(LARGE_COMMAND)
            armed = time.monotonic()
            assert armed - started < 5, "setup too slow to establish simultaneous saturation"
            rejected()

            samples = []
            next_trickle = started + 5
            next_rejection = started + 5
            while time.monotonic() < armed + CLIENT_TIMEOUT + 3:
                now = time.monotonic()
                if now >= next_trickle and now < started + 26:
                    for client in writers:
                        client.sock.sendall(b" ")
                    next_trickle += 5
                if now >= next_rejection and now < started + 20:
                    rejected()
                    next_rejection += 5
                request_started = time.monotonic()
                assert healthy.request("status")["ready"]
                samples.append((time.monotonic() - request_started) * 1000)
                time.sleep(0.2)

            # Old sockets remain open locally, and slow readers have not read
            # their responses. Admitting a complete new set proves that every
            # old handler released its permit, not merely that one slot opened.
            replacements = [connect() for _ in range(MAX_CLIENTS - 1)]
            rejected()
            assert healthy.request("status")["ready"]
            assert all(client.drain_closed() == 0 for client in idle + writers)
            partial_sizes = [client.drain_closed() for client in readers]
            assert all(0 < count < len(LARGE_COMMAND) for count in partial_sizes), partial_sizes
            assert logfile.read_text().count("control client timed out") >= MAX_CLIENTS - 1

            # Peer disconnect must also free a permit immediately, without
            # waiting another 30 seconds for the idle-client deadline.
            replacements[0].close()
            replacement = connect(retry=True)
            assert replacement.request("status")["ready"]
            rejected()

            # The remaining 63 clients stay connected during orderly shutdown.
            shutdown_started = time.monotonic()
            assert healthy.request("shutdown")["accepted"]
            assert process.wait(timeout=6) == 0
            shutdown_seconds = time.monotonic() - shutdown_started
            assert all(client.drain_closed() == 0 for client in replacements[1:] + [replacement])
            print(json.dumps({
                "test": "control-clients", "result": "PASS", "max_clients": MAX_CLIENTS,
                "idle": len(idle), "slow_writers": len(writers), "blocked_readers": len(readers),
                "timeout_seconds": CLIENT_TIMEOUT, "permits_reused": len(replacements),
                "healthy_status_samples": len(samples), "max_status_ms": round(max(samples), 3),
                "partial_response_bytes": {"min": min(partial_sizes), "max": max(partial_sizes)},
                "shutdown_seconds": round(shutdown_seconds, 3),
            }), flush=True)
        except BaseException:
            if logfile.exists():
                print(logfile.read_text(), file=sys.stderr)
            raise
        finally:
            for client in clients:
                client.close()
            if process is not None and process.poll() is None:
                process.kill()
                process.wait(timeout=5)
            if created:
                run("ip", "netns", "del", namespace)


if __name__ == "__main__":
    main()
