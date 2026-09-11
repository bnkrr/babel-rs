#!/usr/bin/env python3
"""Build Linux binary bundles and publish a checked version tag to GitHub.

check/build/smoke never upload. Only publish, inside a new-tag CI job, writes
to GitHub. crates.io publication is handled independently by publish.py.
"""
import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import tarfile
import tempfile

from publish import (CARGO, ROOT, checked_commit, require_ci_tag_push,
                     require_source_tag, workspace_version)

TARGETS = ("x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl")
FILES = {
    "README.md": "packaging/README.md",
    "LICENSE": "LICENSE",
    "examples/babel-rs.toml": "examples/babel-rs.toml",
    "packaging/systemd/babel-rs.service": "packaging/systemd/babel-rs.service",
}


def digest(data):
    return hashlib.sha256(data).hexdigest()


def validate_ref(ref, version, dry_run=False):
    if ref == f"refs/tags/v{version}" or (dry_run and ref.startswith("refs/heads/")):
        return
    raise ValueError(f"binary releases require refs/tags/v{version}; got {ref}")


def changelog_notes(changelog, version, repository, commit):
    """Select one release section; resolve document links at its source commit."""
    sections, body, selected, fence = 0, [], False, None
    heading = re.compile(r"^## (?:\[" + re.escape(version) + r"\]|" + re.escape(version) + r")(?=\s|$)")
    for line in changelog.splitlines():
        marker = re.match(r"^\s{0,3}(`{3,}|~{3,})", line)
        in_code = fence is not None or marker is not None
        if marker:
            token = marker[1]
            if fence is None:
                fence = token
            elif token[0] == fence[0] and len(token) >= len(fence) and not line[marker.end():].strip():
                fence = None
        if not in_code and line.startswith("## "):
            selected = bool(heading.match(line))
            sections += int(selected)
            continue
        if selected:
            if not in_code:
                line = re.sub(r"(\]\()((?:docs|crates|packaging|examples)/[^)\s]+)(\))",
                              lambda m: f"{m[1]}https://github.com/{repository}/blob/{commit}/{m[2]}{m[3]}", line)
            body.append(line)
    notes = "\n".join(body).strip()
    if sections != 1 or not notes:
        raise ValueError(f"CHANGELOG.md must contain one nonempty section for {version}")
    return notes + f"\n\nSource commit: `{commit}`.\n"


def archive_name(version, target):
    return f"babel-rs-{version}-{target}.tar.gz"


def pack(binary, root, output, version, target, commit, timestamp):
    contents = {name: (root / source).read_bytes() for name, source in FILES.items()}
    contents["babel-rs"] = binary.read_bytes()
    contents["build-info.json"] = (json.dumps({
        "version": version, "target": target, "commit": commit,
        "binary_sha256": digest(contents["babel-rs"]),
    }, sort_keys=True, indent=2) + "\n").encode()
    output.mkdir(parents=True, exist_ok=True)
    bundle = output / archive_name(version, target)
    prefix = bundle.name.removesuffix(".tar.gz")
    with bundle.open("wb") as stream, gzip.GzipFile(fileobj=stream, mode="wb", filename="", mtime=0) as zipped:
        with tarfile.open(fileobj=zipped, mode="w") as archive:
            for name, data in sorted(contents.items()):
                member = tarfile.TarInfo(f"{prefix}/{name}")
                member.size, member.mtime = len(data), timestamp
                member.mode = 0o755 if name == "babel-rs" else 0o644
                archive.addfile(member, io.BytesIO(data))
    return bundle


def unpack(bundle, version, target, commit):
    """Read only the expected regular files, never extract archive paths."""
    prefix = archive_name(version, target).removesuffix(".tar.gz")
    expected = {f"{prefix}/{name}" for name in (*FILES, "babel-rs", "build-info.json")}
    with tarfile.open(bundle, "r:gz") as archive:
        members = archive.getmembers()
        if (len(members) != len(expected) or {m.name for m in members} != expected
                or any(not m.isfile() for m in members)):
            raise ValueError(f"unexpected bundle contents: {bundle.name}")
        contents = {m.name[len(prefix) + 1:]: archive.extractfile(m).read() for m in members}
    info = json.loads(contents["build-info.json"])
    if info != {"version": version, "target": target, "commit": commit,
                "binary_sha256": digest(contents["babel-rs"])}:
        raise ValueError(f"bundle provenance or binary checksum mismatch: {bundle.name}")
    return contents


def cargo_executable(output):
    executables = set()
    for line in output.splitlines():
        message = json.loads(line)
        if (message.get("reason") == "compiler-artifact"
                and message["target"]["name"] == "babel-rs"
                and "bin" in message["target"]["kind"] and message.get("executable")):
            executables.add(message["executable"])
    if len(executables) != 1:
        raise ValueError("Cargo did not report exactly one daemon executable")
    return Path(executables.pop())


def build(target, output, version, commit):
    env = dict(os.environ)
    flags = (env["CARGO_ENCODED_RUSTFLAGS"].split("\x1f") if env.get("CARGO_ENCODED_RUSTFLAGS")
             else shlex.split(env.get("RUSTFLAGS", "")))
    # Release artifacts should not embed build-host source/cache paths.
    cargo_home = Path(env.get("CARGO_HOME", Path.home() / ".cargo")).resolve()
    sysroot = subprocess.check_output([env.get("RUSTC", "rustc"), "--print", "sysroot"],
                                      cwd=ROOT, env=env, text=True).strip()
    prefixes = [(str(Path.home()), "home"), (str(ROOT), "."),
                (str(cargo_home), "cargo"), (sysroot, "rust")]
    flags += [f"--remap-path-prefix={source}={replacement}" for source, replacement in prefixes]
    env["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(flags)
    result = subprocess.check_output([CARGO, "build", "--release", "--locked", "-p", "babel-rs",
                                      "--target", target, "--message-format=json-render-diagnostics"],
                                     cwd=ROOT, env=env, text=True)
    binary = cargo_executable(result)
    headers = subprocess.check_output(["readelf", "-l", str(binary)], text=True)
    dynamic = subprocess.check_output(["readelf", "-d", str(binary)], text=True)
    if "INTERP" in headers or "(NEEDED)" in dynamic:
        raise ValueError("musl binary unexpectedly requires a dynamic loader or library")
    binary_bytes = binary.read_bytes()
    if any(source.encode() in binary_bytes for source, _ in prefixes):
        raise ValueError("binary still contains a build-host source, cache or toolchain path")
    run_binary(binary, ROOT / "examples/babel-rs.toml", version)
    timestamp = int(subprocess.check_output(["git", "show", "-s", "--format=%ct", commit], cwd=ROOT))
    bundle = pack(binary, ROOT, output, version, target, commit, timestamp)
    unpack(bundle, version, target, commit)
    print(f"Bundle: {bundle}; sha256: {digest(bundle.read_bytes())}", flush=True)


def run_binary(binary, config, version):
    actual = subprocess.check_output([str(binary), "--version"], text=True).strip()
    if actual != f"babel-rs {version}":
        raise ValueError(f"unexpected daemon version: {actual}")
    subprocess.run([str(binary), "check", "--config", str(config)], check=True)


def smoke(assets, target, version, commit):
    contents = unpack(assets / archive_name(version, target), version, target, commit)
    with tempfile.TemporaryDirectory(prefix="babel-binary-smoke-") as directory:
        root = Path(directory)
        binary, config = root / "babel-rs", root / "babel-rs.toml"
        binary.write_bytes(contents["babel-rs"])
        binary.chmod(0o755)
        config.write_bytes(contents["examples/babel-rs.toml"])
        run_binary(binary, config, version)
        privilege = [] if os.geteuid() == 0 else ["sudo"]
        subprocess.run([*privilege, "sh", str(ROOT / "tests/e2e/netns-three-node.sh"), str(binary)], check=True)


def release_assets(assets, version, commit):
    expected = {archive_name(version, target) for target in TARGETS}
    if {p.name for p in assets.iterdir()} - {"SHA256SUMS"} != expected:
        raise ValueError("release requires exactly the two supported Linux bundles")
    for target in TARGETS:
        unpack(assets / archive_name(version, target), version, target, commit)
    bundles = [assets / name for name in sorted(expected)]
    checksums = assets / "SHA256SUMS"
    checksums.write_text("".join(f"{digest(p.read_bytes())}  {p.name}\n" for p in bundles))
    return bundles + [checksums]


def gh(*args):
    return subprocess.check_output(["gh", *args], cwd=ROOT)


def find_release(repository, tag):
    pages = json.loads(gh("api", "--paginate", "--slurp", f"repos/{repository}/releases?per_page=100"))
    matches = [release for page in pages for release in page if release["tag_name"] == tag]
    if len(matches) > 1:
        raise ValueError("multiple GitHub releases identify the same tag")
    return matches[0] if matches else None


def publish_release(repository, tag, commit, notes, assets):
    """Resume a matching draft, or verify an already published release unchanged."""
    def check_remote_tag():
        actual = gh("api", f"repos/{repository}/commits/{tag}", "--jq", ".sha").decode().strip()
        if actual != commit:
            raise ValueError("remote version tag no longer identifies the tested commit")

    check_remote_tag()
    release = find_release(repository, tag)
    if release is None:
        with tempfile.TemporaryDirectory(prefix="babel-release-notes-") as directory:
            path = Path(directory) / "notes.md"
            path.write_text(notes)
            gh("release", "create", tag, "--repo", repository, "--verify-tag", "--draft",
               "--title", tag, "--notes-file", str(path))
        release = find_release(repository, tag)
    if release is None or (release.get("body") or "").strip() != notes.strip() or release.get("prerelease"):
        raise ValueError("existing release notes or status differ; inspect before continuing")
    expected = {p.name: p for p in assets}
    existing = {a["name"]: a for a in release["assets"]}
    if len(existing) != len(release["assets"]) or existing.keys() - expected.keys():
        raise ValueError("existing release contains unexpected assets; refusing to replace them")
    for name, asset in existing.items():
        remote = gh("api", f"repos/{repository}/releases/assets/{asset['id']}",
                    "-H", "Accept: application/octet-stream")
        if digest(remote) != digest(expected[name].read_bytes()):
            raise ValueError(f"existing asset differs: {name}; refusing to overwrite")
    missing = [str(path) for name, path in expected.items() if name not in existing]
    if not release["draft"]:
        if missing:
            raise ValueError("published release is incomplete; refusing to change it")
        print(f"{tag}: already published with matching source, notes and assets")
        return
    if missing:
        gh("release", "upload", tag, *missing, "--repo", repository)
        uploaded = find_release(repository, tag)
        if (uploaded is None or uploaded["id"] != release["id"] or not uploaded["draft"]
                or (uploaded.get("body") or "").strip() != notes.strip()
                or {a["name"] for a in uploaded["assets"]} != expected.keys()
                or len(uploaded["assets"]) != len(expected)):
            raise ValueError("draft changed or assets are incomplete after upload")
        for asset in uploaded["assets"]:
            remote = gh("api", f"repos/{repository}/releases/assets/{asset['id']}",
                        "-H", "Accept: application/octet-stream")
            if digest(remote) != digest(expected[asset["name"]].read_bytes()):
                raise ValueError("uploaded asset checksum mismatch; leaving release as a draft")
    check_remote_tag()
    gh("api", "--method", "PATCH", f"repos/{repository}/releases/{release['id']}",
       "-F", "draft=false", "-f", "make_latest=legacy")
    print(f"{tag}: GitHub Release published", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("check", "build", "smoke", "publish"))
    parser.add_argument("--ref", default=os.environ.get("GITHUB_REF", ""))
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--target", choices=TARGETS)
    parser.add_argument("--assets", type=Path, default=ROOT / ".local/experiments/release/assets")
    args = parser.parse_args()
    if args.dry_run and args.command != "check":
        parser.error("--dry-run is only valid for check; build and smoke never upload")
    version = workspace_version(ROOT)
    commit = checked_commit(ROOT)
    repository = os.environ.get("GITHUB_REPOSITORY", "bnkrr/babel-rs")
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
        raise ValueError("invalid GitHub repository")
    notes = changelog_notes((ROOT / "CHANGELOG.md").read_text(), version, repository, commit)
    if args.command in ("check", "publish"):
        validate_ref(args.ref, version, args.dry_run)
        if not args.dry_run:
            require_source_tag(ROOT, version, commit)
    if args.command == "publish":
        require_ci_tag_push(args.ref, commit)
        assets = release_assets(args.assets, version, commit)
        publish_release(repository, f"v{version}", commit, notes, assets)
    elif args.command in ("build", "smoke"):
        if not args.target:
            parser.error("build and smoke require --target")
        if args.command == "build":
            build(args.target, args.assets, version, commit)
        else:
            smoke(args.assets, args.target, version, commit)
    else:
        print(f"Binary release: {version}; source: {commit}; uploads: disabled")


if __name__ == "__main__":
    main()
