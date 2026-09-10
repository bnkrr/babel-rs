#!/usr/bin/env python3
"""Verify archives and independent consumers without publishing any package.

Requires Cargo with workspace packaging (tested with 1.96), Python 3.11+,
and the workspace dependencies in the cache when --offline is selected.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import tomllib


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--offline", action="store_true")
    parser.add_argument("--allow-dirty", action="store_true")
    parser.add_argument("--output", type=Path, help="retain archives and build artifacts here")
    args = parser.parse_args()
    repo = Path(__file__).resolve().parents[2]
    cargo = os.environ.get("CARGO", "cargo")
    manifest = tomllib.loads((repo / "Cargo.toml").read_text())
    version = manifest["workspace"]["package"]["version"]
    names = ("babel-protocol", "babel-router", "babel-rs")
    with tempfile.TemporaryDirectory(prefix="babel-package-check-") as temporary:
        output = (args.output or Path(temporary)).resolve()
        output.mkdir(parents=True, exist_ok=True)
        environment = dict(os.environ, CARGO_TARGET_DIR=str(output / "target"))

        def run(*command, cwd=output):
            subprocess.run(command, cwd=cwd, env=environment, check=True)

        offline = ["--offline"] if args.offline else []
        dirty = ["--allow-dirty"] if args.allow_dirty else []
        # Cargo caches staged registry packages by name/version. Give every
        # rehearsal a fresh staging registry so editing an unpublished version
        # cannot accidentally verify a previous run's dependency contents.
        package_target = Path(temporary) / "packaging"
        run(cargo, "package", "--workspace", "--locked", "--target-dir", str(package_target), *offline, *dirty, cwd=repo)
        archives = output / "archives"
        archives.mkdir(exist_ok=True)
        extracted = Path(temporary) / "unpacked"
        extracted.mkdir(exist_ok=True)
        for name in names:
            archive = package_target / "package" / f"{name}-{version}.crate"
            shutil.copy2(archive, archives / archive.name)
            with tarfile.open(archive) as package:
                files = set(package.getnames())
                prefix = f"{name}-{version}/"
                for required in ("README.md", "LICENSE", "Cargo.toml", "Cargo.lock"):
                    assert prefix + required in files, f"{name}: missing {required}"
                if name == "babel-rs":
                    assert prefix + "examples/babel-rs.toml" in files
                assert not any("/.local/" in f or "/.git/" in f for f in files)
                package.extractall(extracted, filter="data")
            assert (extracted / f"{name}-{version}/LICENSE").read_bytes() == (repo / "LICENSE").read_bytes()

        # Only released archives are patched into this independent consumer.
        # No dependency points into the original repository or its crate directories.
        patches = Path(temporary) / "archive-dependencies.toml"
        patches.write_text("[patch.crates-io]\n" + "".join(
            f"{name} = {{ path = {json.dumps(str(extracted / f'{name}-{version}'))} }}\n"
            for name in names[:2]))
        consumer = Path(temporary) / "consumer"
        (consumer / "src").mkdir(parents=True, exist_ok=True)
        tokio_version = manifest["workspace"]["dependencies"]["tokio"]["version"]
        (consumer / "Cargo.toml").write_text(f'''[package]
name = "babel-external-consumer"
version = "0.0.0"
edition = "2024"
[dependencies]
babel-protocol = "={version}"
babel-router = "={version}"
tokio = {{ version = "{tokio_version}", features = ["rt-multi-thread", "macros"] }}
''')
        (consumer / "src/main.rs").write_text('''use babel_protocol::{Engine, EngineConfig, Event, RouteKey, RouterId};
use babel_router::{BabelRouter, Ipv4NextHop};
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let id = RouterId::new([7;8]).unwrap();
    let key = RouteKey::new("2001:db8::/64".parse()?, None).unwrap();
    let mut engine = Engine::try_new(EngineConfig::recommended(id))?;
    engine.try_handle(Event::Originate {key, metric:0, now_ms:0})?;
    assert_eq!(Ipv4NextHop::default(), Ipv4NextHop::Auto);
    let router = BabelRouter::builder().router_id(id).sequence_number(100).start().await?;
    let handle = router.handle();
    handle.originate(key,0).await?;
    handle.withdraw(key).await?;
    handle.replace_origins(vec![(key,0)]).await?;
    assert!(handle.status().await?.interfaces.is_empty());
    router.shutdown().await?;
    assert!(handle.status().await.is_err());
    Ok(())
}
''')
        run(cargo, "run", "--manifest-path", str(consumer / "Cargo.toml"), "--config", str(patches), *offline)
        for name in names[:2]:
            crate = extracted / f"{name}-{version}/Cargo.toml"
            run(cargo, "test", "--manifest-path", str(crate), "--all-targets", "--config", str(patches), *offline)
            run(cargo, "test", "--manifest-path", str(crate), "--doc", "--config", str(patches), *offline)
            run(cargo, "doc", "--manifest-path", str(crate), "--no-deps", "--config", str(patches), *offline)
        run(cargo, "install", "--path", str(extracted / f"babel-rs-{version}"), "--root", str(output / "install"),
            "--locked", "--force", "--config", str(patches), *offline)
        binary = output / "install/bin/babel-rs"
        run(str(binary), "check", "--config", str(extracted / f"babel-rs-{version}/examples/babel-rs.toml"))
        run(str(binary), "--version")
        print(json.dumps({"result": "PASS", "version": version, "packages": names,
                          "consumer": "archives only", "uploaded": False}), flush=True)


if __name__ == "__main__":
    main()
