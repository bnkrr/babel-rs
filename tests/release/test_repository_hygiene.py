"""Keep machine-specific home directories out of published source and guides."""
from pathlib import Path
import re
import subprocess
import unittest


class RepositoryHygieneTests(unittest.TestCase):
    def test_tracked_files_do_not_embed_machine_home_paths(self):
        repo = Path(__file__).resolve().parents[2]
        paths = subprocess.check_output(["git", "ls-files", "-z"], cwd=repo).split(b"\0")
        home_path = re.compile(rb"/(?:root|(?:home|Users)/[^/\s\"']+)/")
        findings = []
        for raw in paths:
            if not raw:
                continue
            path = Path(raw.decode())
            content = (repo / path).read_bytes()
            if b"\0" in content:
                continue
            for number, line in enumerate(content.splitlines(), 1):
                if home_path.search(line):
                    findings.append(f"{path}:{number}")
        self.assertEqual(findings, [], "Use configurable or repository-relative paths: "
                         + ", ".join(findings))


if __name__ == "__main__":
    unittest.main()
