"""Run the VM entry points against fake tools; no network or privileges needed."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


REPO = Path(__file__).resolve().parents[2]


class VmWrapperTests(unittest.TestCase):
    def run_wrapper(self, suite, overrides=False):
        with tempfile.TemporaryDirectory(prefix="babel-wrapper-") as temporary:
            root = Path(temporary)
            log = root / "commands.jsonl"
            target = root / "build output"
            tools = root / "tools"
            tools.mkdir()
            stub = f"#!{sys.executable}\n" + '''import json, os, sys
from pathlib import Path
entry = {"tool": Path(sys.argv[0]).name, "args": sys.argv[1:],
         "cwd": os.getcwd(), "env": {key: os.environ.get(key) for key in
         ("CARGO_HOME", "CARGO_TARGET_DIR", "RUSTUP_TOOLCHAIN")}}
with open(os.environ["BABEL_WRAPPER_LOG"], "a") as log:
    log.write(json.dumps(entry) + "\\n")
if sys.argv[1:2] == ["metadata"]:
    print(json.dumps({"target_directory": os.environ["BABEL_WRAPPER_TARGET"]}))
'''
            for name in ("cargo", "custom-cargo", "ssh", "scp"):
                tool = tools / name
                tool.write_text(stub)
                tool.chmod(0o755)
            env = {key: value for key, value in os.environ.items()
                   if not key.startswith(("BABEL_RS_", "CARGO_", "RUSTUP_TOOLCHAIN"))}
            env.update(PATH=str(tools) + os.pathsep + env.get("PATH", ""),
                       BABEL_WRAPPER_LOG=str(log), BABEL_WRAPPER_TARGET=str(target),
                       BABEL_RS_E2E_HOST="test-vm", PYTHONDONTWRITEBYTECODE="1")
            remote = f"/tmp/babel-rs-{suite}"
            if overrides:
                remote = "/var/tmp/test-assets"
                env.update(CARGO_HOME=str(root / "cargo cache"),
                           CARGO_TARGET_DIR=str(target), RUSTUP_TOOLCHAIN="test-toolchain",
                           BABEL_RS_CARGO_BIN=str(tools / "custom-cargo"),
                           BABEL_RS_SSH_CONFIG=str(root / "ssh config"))
                env[f"BABEL_RS_{'ENDLESS' if suite == 'endless' else 'E2E'}_REMOTE_ROOT"] = remote
            args = ["--nodes", "3", "--rounds", "1"] if suite == "endless" else ["route-policy"]
            subprocess.run(["bash", str(REPO / "tests" / suite / "run-on-linux-vm.sh"), *args],
                           cwd=root, env=env, check=True, capture_output=True, text=True)
            entries = [json.loads(line) for line in log.read_text().splitlines()]
            cargo = [entry for entry in entries if entry["tool"].endswith("cargo")]
            self.assertTrue(cargo)
            for entry in cargo:
                self.assertEqual(entry["cwd"], str(REPO))
                for key in ("CARGO_HOME", "CARGO_TARGET_DIR", "RUSTUP_TOOLCHAIN"):
                    self.assertEqual(entry["env"][key], env.get(key))
                if entry["args"][0] == "build":
                    self.assertIn("--locked", entry["args"])
            copied = next(entry["args"] for entry in entries if entry["tool"] == "scp")
            self.assertIn(str(target / "release/babel-rs"), copied)
            if suite == "e2e":
                self.assertIn(str(target / "release/examples/route_policy"), copied)
            self.assertEqual(copied[-1], "test-vm:" + remote + "/")
            ssh = [entry["args"] for entry in entries if entry["tool"] == "ssh"]
            self.assertIn(remote + "/babel-rs", ssh[-1][-1])
            for command in [*ssh, copied]:
                self.assertEqual("-F" in command, overrides)
                if overrides:
                    self.assertEqual(command[command.index("-F") + 1], env["BABEL_RS_SSH_CONFIG"])

    def test_default_tools_and_directories(self):
        for suite in ("e2e", "endless"):
            with self.subTest(suite=suite):
                self.run_wrapper(suite)

    def test_caller_environment_and_custom_paths(self):
        for suite in ("e2e", "endless"):
            with self.subTest(suite=suite):
                self.run_wrapper(suite, overrides=True)


if __name__ == "__main__":
    unittest.main()
