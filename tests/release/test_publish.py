"""Release orchestration regressions; no network access or real uploads."""
import importlib.util
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import urllib.error

spec = importlib.util.spec_from_file_location("publish", Path(__file__).resolve().parents[2] / "scripts/publish.py")
publish = importlib.util.module_from_spec(spec)
spec.loader.exec_module(publish)


def archive(commit="abc", dirty=False):
    buffer = io.BytesIO()
    contents = json.dumps({"git": {"sha1": commit, "dirty": dirty}}).encode()
    with tarfile.open(fileobj=buffer, mode="w:gz") as output:
        member = tarfile.TarInfo("babel-protocol-0.5.0/.cargo_vcs_info.json")
        member.size = len(contents)
        output.addfile(member, io.BytesIO(contents))
    return buffer.getvalue()


class ReleaseTests(unittest.TestCase):
    def test_upload_requires_matching_tag_and_dry_run_checks_tag_too(self):
        publish.validate_ref("refs/tags/v0.5.0", "0.5.0")
        publish.validate_ref("refs/heads/main", "0.5.0", dry_run=True)
        for ref, dry in [("refs/heads/main", False), ("refs/tags/v0.4.0", False), ("refs/tags/v0.4.0", True)]:
            with self.assertRaises(ValueError):
                publish.validate_ref(ref, "0.5.0", dry)

    def test_checks_never_upload_or_request_registry_credentials(self):
        with patch("sys.argv", ["publish.py", "check", "--ref", "refs/heads/main", "--dry-run"]), \
             patch.object(publish, "publish") as upload, patch.object(publish, "get") as request:
            publish.main()
        upload.assert_not_called()
        request.assert_not_called()

    def test_dirty_or_wrong_commit_cannot_upload(self):
        for git_output in [[" M file"], ["", "unexpected-commit"]]:
            with patch("sys.argv", ["publish.py", "publish", "--ref", "refs/tags/v0.5.0"]), \
                 patch.object(publish, "workspace_version", return_value="0.5.0"), \
                 patch.object(publish.subprocess, "check_output", side_effect=git_output), \
                 patch.dict(publish.os.environ, {"GITHUB_SHA": "expected-commit"}), \
                 patch.object(publish, "publish") as upload, \
                 patch("sys.stderr", new=io.StringIO()), self.assertRaises(SystemExit):
                publish.main()
            upload.assert_not_called()

    def test_version_overrides_and_internal_dependencies_must_match(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            root_manifest = '[workspace.package]\nversion = "0.5.0"\n[workspace.dependencies]\nbabel-protocol = { version = "0.5.0" }\nbabel-router = { version = "0.5.0" }\n'
            (root / "Cargo.toml").write_text(root_manifest)
            for name in publish.PACKAGES:
                folder = root / "crates" / name
                folder.mkdir(parents=True)
                (folder / "Cargo.toml").write_text(f'[package]\nname = "{name}"\nversion.workspace = true\n')
            self.assertEqual(publish.workspace_version(root), "0.5.0")
            member = root / "crates/babel-router/Cargo.toml"
            original = member.read_text()
            member.write_text(original.replace('version.workspace = true', 'version = "0.4.0"'))
            with self.assertRaises(ValueError):
                publish.workspace_version(root)
            member.write_text(original)
            (root / "Cargo.toml").write_text(root_manifest.replace('babel-router = { version = "0.5.0" }', 'babel-router = { version = "0.4.0" }'))
            with self.assertRaises(ValueError):
                publish.workspace_version(root)

    def test_only_404_means_absent(self):
        for code in (401, 403, 429, 500):
            with patch.object(publish.urllib.request, "urlopen", side_effect=urllib.error.HTTPError("url", code, "error", {}, None)):
                with self.assertRaises(urllib.error.HTTPError):
                    publish.registry_version("babel-protocol", "0.5.0")
        with patch.object(publish.urllib.request, "urlopen", side_effect=urllib.error.HTTPError("url", 404, "missing", {}, None)):
            self.assertIsNone(publish.registry_version("babel-protocol", "0.5.0"))

    def test_resume_requires_same_clean_source_and_active_version(self):
        metadata = {"num": "0.5.0", "yanked": False}
        with patch.object(publish, "get", return_value=archive()):
            publish.verify_existing("babel-protocol", "0.5.0", "abc", metadata)
            for other in [dict(metadata, yanked=True), dict(metadata, num="0.4.0")]:
                with self.assertRaises(ValueError):
                    publish.verify_existing("babel-protocol", "0.5.0", "abc", other)
        for contents in [archive("different"), archive(dirty=True)]:
            with patch.object(publish, "get", return_value=contents), self.assertRaises(ValueError):
                publish.verify_existing("babel-protocol", "0.5.0", "abc", metadata)

    def test_partial_release_resumes_in_dependency_order(self):
        operations = []
        with patch.object(publish, "registry_version", side_effect=[{"num": "0.5.0"}, None, None]), \
             patch.object(publish, "verify_existing", side_effect=lambda name, *args: operations.append(("existing", name))), \
             patch.object(publish.subprocess, "run", side_effect=lambda command, **kwargs: operations.append(("upload", command[3]))), \
             patch.object(publish, "wait_for_index", side_effect=lambda name, _: operations.append(("index", name))):
            publish.publish("0.5.0", "abc")
        self.assertEqual(operations, [("existing", "babel-protocol"), ("index", "babel-protocol"),
                                     ("upload", "babel-router"), ("index", "babel-router"),
                                     ("upload", "babel-rs"), ("index", "babel-rs")])

    def test_failure_does_not_publish_dependents(self):
        with patch.object(publish, "registry_version", return_value=None), \
             patch.object(publish.subprocess, "run", side_effect=RuntimeError("upload failed")) as upload, \
             patch.object(publish, "wait_for_index") as wait:
            with self.assertRaises(RuntimeError):
                publish.publish("0.5.0", "abc")
        self.assertEqual(upload.call_count, 1)
        wait.assert_not_called()

    def test_waits_for_exact_index_version_and_rejects_yank(self):
        with patch.object(publish, "get", side_effect=[b'{"vers":"0.4.0"}', b'{"vers":"0.5.0","yanked":false}']), \
             patch.object(publish.time, "sleep") as sleep:
            publish.wait_for_index("babel-protocol", "0.5.0")
            sleep.assert_called_once_with(5)
        with patch.object(publish, "get", return_value=b'{"vers":"0.5.0","yanked":true}'), self.assertRaises(ValueError):
            publish.wait_for_index("babel-protocol", "0.5.0")
        with patch.object(publish, "get", return_value=None), self.assertRaises(TimeoutError):
            publish.wait_for_index("babel-protocol", "0.5.0", timeout=0)


if __name__ == "__main__":
    unittest.main()
