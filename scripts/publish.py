#!/usr/bin/env python3
"""Release checks, explicit publishing, and unpatched registry verification.

`check` never uploads. `publish` is intended for the tested tag's CI job.
"""
import argparse
import io
import json
import os
from pathlib import Path
import re
import subprocess
import tarfile
import tempfile
import time
import tomllib
import urllib.error
import urllib.request

PACKAGES = ("babel-protocol", "babel-router", "babel-rs")
ROOT = Path(__file__).resolve().parents[1]
CARGO = os.environ.get("CARGO", "cargo")
VERSION = re.compile(r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\Z")


def workspace_version(root):
    workspace = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]
    version = workspace["package"]["version"]
    if not VERSION.fullmatch(version):
        raise ValueError("release automation requires a numeric MAJOR.MINOR.PATCH version")
    for name in PACKAGES:
        package = tomllib.loads((root / "crates" / name / "Cargo.toml").read_text())["package"]
        if package["name"] != name or package["version"] not in (version, {"workspace": True}):
            raise ValueError(f"{name}: package name/version does not match release {version}")
    for name in PACKAGES[:-1]:
        if workspace["dependencies"][name]["version"] != version:
            raise ValueError(f"{name}: workspace dependency does not match release {version}")
    return version


def validate_ref(ref, version, dry_run=False):
    expected = f"refs/tags/publish/v{version}"
    if ref == expected:
        return
    if dry_run and (ref == f"refs/tags/v{version}" or ref.startswith("refs/heads/")):
        return
    raise ValueError(f"crate publication requires {expected}; got {ref}")


def checked_commit(root):
    if subprocess.check_output(["git", "status", "--porcelain"], cwd=root, text=True).strip():
        raise ValueError("release artifacts require a clean checkout")
    return subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()


def require_source_tag(root, version, commit):
    source = subprocess.check_output(
        ["git", "rev-parse", "--verify", f"refs/tags/v{version}^{{commit}}"], cwd=root, text=True).strip()
    if source != commit:
        raise ValueError(f"v{version} must identify the checked-out source commit")


def require_ci_tag_push(ref, commit):
    if (os.environ.get("GITHUB_EVENT_NAME") != "push"
            or os.environ.get("GITHUB_REF") != ref
            or os.environ.get("GITHUB_SHA") != commit):
        raise ValueError("uploads require the CI job's tag-push ref and checked-out commit")
    event_path = os.environ.get("GITHUB_EVENT_PATH")
    if not event_path:
        raise ValueError("missing CI push event")
    event = json.loads(Path(event_path).read_text())
    if (event.get("ref") != ref or event.get("created") is not True
            or event.get("deleted") is not False or event.get("forced") is not False):
        raise ValueError("uploads require a newly created tag, not a moved or deleted tag")


def get(url, missing_ok=False):
    request = urllib.request.Request(url, headers={"User-Agent": "babel-rs-release (github.com/bnkrr/babel-rs)"})
    try:
        with urllib.request.urlopen(request, timeout=20) as response:
            return response.read()
    except urllib.error.HTTPError as error:
        if missing_ok and error.code == 404:
            return None
        raise


def registry_version(name, version):
    body = get(f"https://crates.io/api/v1/crates/{name}/{version}", missing_ok=True)
    return json.loads(body)["version"] if body is not None else None


def verify_existing(name, version, commit, metadata):
    if metadata.get("yanked") or metadata.get("num") != version:
        raise ValueError(f"{name} {version}: registry version is yanked or mismatched")
    body = get(f"https://static.crates.io/crates/{name}/{name}-{version}.crate")
    with tarfile.open(fileobj=io.BytesIO(body), mode="r:gz") as archive:
        vcs = archive.extractfile(f"{name}-{version}/.cargo_vcs_info.json")
        if vcs is None:
            raise ValueError(f"{name} {version}: missing source commit metadata")
        git = json.load(vcs)["git"]
    if git.get("sha1") != commit or git.get("dirty", False):
        raise ValueError(f"{name} {version}: already published from different or dirty source; inspect before continuing")


def wait_for_index(name, version, timeout=180):
    deadline = time.monotonic() + timeout
    while True:
        body = get(f"https://index.crates.io/{name[:2]}/{name[2:4]}/{name}", missing_ok=True)
        if body:
            for line in body.splitlines():
                entry = json.loads(line)
                if entry["vers"] == version:
                    if entry.get("yanked"):
                        raise ValueError(f"{name} {version} is yanked")
                    return
        if time.monotonic() >= deadline:
            raise TimeoutError(f"{name} {version} did not appear in the index; inspect registry before retrying")
        time.sleep(5)


def publish(version, commit):
    for name in PACKAGES:
        metadata = registry_version(name, version)
        if metadata is not None:
            verify_existing(name, version, commit, metadata)
            print(f"{name} {version}: already published from {commit}; resuming", flush=True)
        else:
            subprocess.run([CARGO, "publish", "-p", name, "--locked", "--registry", "crates-io"], cwd=ROOT, check=True)
        wait_for_index(name, version)


def verify_consumers(version):
    for name in PACKAGES:
        wait_for_index(name, version)
    with tempfile.TemporaryDirectory(prefix="babel-registry-consumer-") as directory:
        root = Path(directory)
        (root / "src").mkdir()
        (root / "Cargo.toml").write_text(f'''[package]
name = "babel-registry-consumer"
version = "0.0.0"
edition = "2024"
[dependencies]
babel-protocol = "={version}"
babel-router = "={version}"
''')
        (root / "src/lib.rs").write_text('''pub fn validate_runtime() -> Result<(), babel_router::RouterError> {
    let id = babel_protocol::RouterId::new([7; 8]).unwrap();
    let _engine = babel_protocol::Engine::try_new(babel_protocol::EngineConfig::recommended(id))?;
    babel_router::BabelRouter::builder().router_id(id).sequence_number(100).validate()
}
''')
        subprocess.run([CARGO, "check", "--manifest-path", str(root / "Cargo.toml")], cwd=root, check=True)
        subprocess.run([CARGO, "install", "babel-rs", "--version", f"={version}", "--locked", "--root", str(root / "install")], cwd=root, check=True)
        binary = root / "install/bin/babel-rs"
        actual = subprocess.check_output([str(binary), "--version"], text=True).strip()
        if actual != f"babel-rs {version}":
            raise ValueError(f"unexpected installed version: {actual}")
        subprocess.run([str(binary), "check", "--config", str(ROOT / "examples/babel-rs.toml")], cwd=root, check=True)
    print(f"Registry consumer and installation PASS: {version}", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("check", "publish", "verify"))
    parser.add_argument("--ref", default=os.environ.get("GITHUB_REF", ""))
    parser.add_argument("--dry-run", action="store_true", help="check a branch or version tag without publishing")
    args = parser.parse_args()
    if args.dry_run and args.command != "check":
        parser.error("--dry-run is only valid for check; publishing requires the explicit publish command")
    version = workspace_version(ROOT)
    if args.command == "verify":
        verify_consumers(version)
        return
    validate_ref(args.ref, version, args.dry_run)
    commit = None
    if not args.dry_run or args.ref.startswith("refs/tags/publish/"):
        commit = checked_commit(ROOT)
        require_source_tag(ROOT, version, commit)
    if args.command == "check":
        print(f"Release version: {version}; ref: {args.ref}; uploads: disabled")
        return
    require_ci_tag_push(args.ref, commit)
    publish(version, commit)


if __name__ == "__main__":
    main()
