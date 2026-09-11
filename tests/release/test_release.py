"""Binary distribution and publication regressions; no network or real uploads."""
import importlib.util
import io
import json
from pathlib import Path
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[2] / "scripts"
sys.path.insert(0, str(SCRIPTS))
spec = importlib.util.spec_from_file_location("binary_release", SCRIPTS / "release.py")
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)
sys.path.pop(0)


class FakeGitHub:
    def __init__(self):
        self.release = None
        self.bytes = {}
        self.operations = []
        self.fail_upload = False
        self.commit = "abc"

    def __call__(self, *args):
        self.operations.append(args)
        if args[:2] == ("api", "--paginate"):
            return json.dumps([[self.release] if self.release else []]).encode()
        if args[0] == "api" and "/commits/" in args[1]:
            return self.commit.encode()
        if args[:2] == ("release", "create"):
            self.release = {"id": 1, "tag_name": args[2], "draft": True, "prerelease": False,
                            "body": Path(args[args.index("--notes-file") + 1]).read_text(), "assets": []}
        elif args[:2] == ("release", "upload"):
            for file in args[3:args.index("--repo")]:
                path = Path(file)
                asset_id = len(self.bytes) + 1
                self.bytes[asset_id] = path.read_bytes()
                self.release["assets"].append({"id": asset_id, "name": path.name})
                if self.fail_upload:
                    raise RuntimeError("connection interrupted during upload")
        elif args[:3] == ("api", "--method", "PATCH"):
            self.release["draft"] = False
        elif args[0] == "api" and "/releases/assets/" in args[1]:
            return self.bytes[int(args[1].rsplit("/", 1)[1])]
        else:
            raise AssertionError(args)
        return b""


class BinaryReleaseTests(unittest.TestCase):
    def test_tag_and_version_namespaces_are_separate(self):
        release.validate_ref("refs/tags/v0.6.0", "0.6.0")
        release.validate_ref("refs/heads/main", "0.6.0", dry_run=True)
        for ref in ("refs/tags/publish/v0.6.0", "refs/tags/v0.6.1", "refs/tags/v0.6.0-release", ""):
            for dry_run in (True, False):
                with self.assertRaises(ValueError):
                    release.validate_ref(ref, "0.6.0", dry_run)
        with self.assertRaises(ValueError):
            release.validate_ref("refs/heads/main", "0.6.0")

    def test_changelog_exact_section_fences_and_source_links(self):
        changelog = '''# Changelog
## Unreleased
Future.
## [0.6.0] — 2026-09-11
Changes with [guide](docs/guide/sadr.md#lookup) and [site](https://example.org).
```md
## 0.6.0
[code](docs/keep.md)
```
### Migration
Details.
## 0.5.0
Older.
'''
        notes = release.changelog_notes(changelog, "0.6.0", "example/babel-rs", "abc")
        self.assertIn("https://github.com/example/babel-rs/blob/abc/docs/guide/sadr.md#lookup", notes)
        self.assertIn("[site](https://example.org)", notes)
        self.assertIn("[code](docs/keep.md)", notes)
        self.assertIn("### Migration\nDetails.", notes)
        self.assertNotIn("Older.", notes)
        self.assertNotIn("Future.", notes)
        self.assertTrue(notes.endswith("Source commit: `abc`.\n"))
        for invalid in ("## 0.6.01\nWrong", "## 0.6.0\n\n## 0.5.0\nOld",
                        "## 0.6.0\nFirst\n## 0.6.0\nSecond"):
            with self.assertRaises(ValueError):
                release.changelog_notes(invalid, "0.6.0", "example/babel-rs", "abc")

    def test_cargo_report_selects_executable_from_custom_target_directory(self):
        report = [
            {"reason": "compiler-artifact", "target": {"name": "babel_protocol", "kind": ["lib"]}},
            {"reason": "compiler-artifact", "target": {"name": "babel-rs", "kind": ["bin"]},
             "executable": "/tmp/custom build/release/babel-rs"},
            {"reason": "build-finished", "success": True},
        ]
        self.assertEqual(release.cargo_executable("\n".join(map(json.dumps, report))),
                         Path("/tmp/custom build/release/babel-rs"))
        with self.assertRaises(ValueError):
            release.cargo_executable(json.dumps(report[0]))

    def test_build_rejects_embedded_toolchain_paths_before_packaging(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "babel-rs"
            toolchain = str(root / "toolchain")
            binary.write_bytes(b"ELF payload: " + toolchain.encode() + b"/library/core/src/ops.rs")
            message = {"reason": "compiler-artifact", "target": {"name": "babel-rs", "kind": ["bin"]},
                       "executable": str(binary)}
            with patch.object(release.subprocess, "check_output",
                              side_effect=[toolchain, json.dumps(message), "no interpreter", "no dynamic dependencies"]), \
                 patch.object(release, "run_binary") as run, patch.object(release, "pack") as pack, \
                 self.assertRaisesRegex(ValueError, "toolchain path"):
                release.build(release.TARGETS[0], root / "output", "0.6.0", "abc")
            run.assert_not_called()
            pack.assert_not_called()

    def bundles(self, root):
        for source in release.FILES.values():
            path = root / source
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(source)
        binary = root / "binary"
        binary.write_bytes(b"test executable")
        output = root / "assets"
        for target in release.TARGETS:
            release.pack(binary, root, output, "0.6.0", target, "abc", 123)
        return binary, output

    def test_archive_contents_checksums_provenance_and_deterministic_metadata(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary, output = self.bundles(root)
            assets = release.release_assets(output, "0.6.0", "abc")
            self.assertEqual(len(assets), 3)
            for bundle in assets[:2]:
                self.assertIn(f"{release.digest(bundle.read_bytes())}  {bundle.name}", assets[2].read_text())
            bundle = output / release.archive_name("0.6.0", release.TARGETS[0])
            original = bundle.read_bytes()
            release.pack(binary, root, output, "0.6.0", release.TARGETS[0], "abc", 123)
            self.assertEqual(bundle.read_bytes(), original)
            with tarfile.open(bundle) as archive:
                for member in archive.getmembers():
                    self.assertEqual((member.uid, member.gid, member.uname, member.gname, member.mtime),
                                     (0, 0, "", "", 123))
                    self.assertEqual(member.mode, 0o755 if member.name.endswith("/babel-rs") else 0o644)
            with self.assertRaises(ValueError):
                release.release_assets(output, "0.6.0", "different")
            bundle.unlink()
            with self.assertRaises(ValueError):
                release.release_assets(output, "0.6.0", "abc")

    def test_tampered_binary_and_unsafe_archive_entries_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            _, output = self.bundles(root)
            target = release.TARGETS[0]
            bundle = output / release.archive_name("0.6.0", target)
            original = bundle.read_bytes()
            for mutation in ("binary", "path", "symlink", "duplicate"):
                with tarfile.open(fileobj=io.BytesIO(original), mode="r:gz") as source, tarfile.open(bundle, "w:gz") as dest:
                    for member in source.getmembers():
                        data = source.extractfile(member).read()
                        if member.name.endswith("/babel-rs"):
                            if mutation == "binary":
                                data = b"tampered"
                                member.size = len(data)
                            elif mutation == "path":
                                member.name = "../escape"
                            elif mutation == "symlink":
                                member.type, member.linkname, member.size = tarfile.SYMTYPE, "../escape", 0
                            elif mutation == "duplicate":
                                dest.addfile(member, io.BytesIO(data))
                        dest.addfile(member, io.BytesIO(data))
                with self.assertRaises(ValueError):
                    release.unpack(bundle, "0.6.0", target, "abc")

    def test_draft_resume_and_published_release_idempotence(self):
        with tempfile.TemporaryDirectory() as directory:
            _, output = self.bundles(Path(directory))
            assets = release.release_assets(output, "0.6.0", "abc")
            fake = FakeGitHub()
            fake.fail_upload = True
            with patch.object(release, "gh", side_effect=fake):
                with self.assertRaises(RuntimeError):
                    release.publish_release("example/babel-rs", "v0.6.0", "abc", "Notes", assets)
                self.assertTrue(fake.release["draft"])
                self.assertEqual(len(fake.release["assets"]), 1)
                fake.fail_upload = False
                release.publish_release("example/babel-rs", "v0.6.0", "abc", "Notes", assets)
                self.assertFalse(fake.release["draft"])
                self.assertEqual(len(fake.release["assets"]), 3)
                fake.operations.clear()
                release.publish_release("example/babel-rs", "v0.6.0", "abc", "Notes", assets)
                self.assertFalse(any(args[0] == "release" or "PATCH" in args for args in fake.operations))

    def test_existing_release_conflicts_and_moved_tag_never_overwrite(self):
        with tempfile.TemporaryDirectory() as directory:
            _, output = self.bundles(Path(directory))
            assets = release.release_assets(output, "0.6.0", "abc")
            for conflict in ("notes", "asset", "missing", "tag"):
                fake = FakeGitHub()
                with patch.object(release, "gh", side_effect=fake):
                    release.publish_release("example/babel-rs", "v0.6.0", "abc", "Notes", assets)
                    if conflict == "notes":
                        fake.release["body"] = "Edited"
                    elif conflict == "asset":
                        fake.bytes[1] = b"other build"
                    elif conflict == "missing":
                        fake.release["assets"].pop()
                    else:
                        fake.commit = "moved"
                    fake.operations.clear()
                    with self.assertRaises(ValueError):
                        release.publish_release("example/babel-rs", "v0.6.0", "abc", "Notes", assets)
                    self.assertFalse(any(args[0] == "release" or "PATCH" in args for args in fake.operations))

    def test_corrupt_upload_or_tag_move_during_upload_leaves_a_draft(self):
        with tempfile.TemporaryDirectory() as directory:
            _, output = self.bundles(Path(directory))
            assets = release.release_assets(output, "0.6.0", "abc")
            for conflict in ("bytes", "tag"):
                fake = FakeGitHub()

                def gh(*args):
                    result = fake(*args)
                    if args[:2] == ("release", "upload"):
                        if conflict == "bytes":
                            fake.bytes[1] = b"corrupted in transit"
                        else:
                            fake.commit = "moved during upload"
                    return result

                with patch.object(release, "gh", side_effect=gh), self.assertRaises(ValueError):
                    release.publish_release("example/babel-rs", "v0.6.0", "abc", "Notes", assets)
                self.assertTrue(fake.release["draft"])

    def test_rehearsal_and_manual_event_cannot_publish(self):
        with patch.object(release, "checked_commit", return_value="abc"), \
             patch.object(release, "publish_release") as upload, \
             patch.object(release, "gh") as request:
            with patch("sys.argv", ["release.py", "check", "--ref", "refs/heads/main", "--dry-run"]):
                release.main()
            with patch("sys.argv", ["release.py", "publish", "--ref", "refs/tags/v0.6.0"]), \
                 patch.object(release, "require_source_tag"), \
                 patch.dict(release.os.environ, {"GITHUB_EVENT_NAME": "workflow_dispatch"}, clear=True), \
                 self.assertRaises(ValueError):
                release.main()
            upload.assert_not_called()
            request.assert_not_called()


if __name__ == "__main__":
    unittest.main()
