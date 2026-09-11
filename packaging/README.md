# Linux binary installation

Each binary bundle contains the `babel-rs` daemon, an example configuration,
a systemd service, the MIT license, and `build-info.json` identifying its version,
source commit, target, and binary SHA-256. Bundles use static musl linking and
are built for Linux x86_64 or ARM64; a Rust toolchain is not needed to run them.

Download the bundle matching `uname -m` and `SHA256SUMS` from the same
[GitHub Release](https://github.com/bnkrr/babel-rs/releases):

| Host architecture | Bundle target |
| --- | --- |
| `x86_64` | `x86_64-unknown-linux-musl` |
| `aarch64` / ARM64 | `aarch64-unknown-linux-musl` |

For example, with the 0.6.0 x86_64 bundle and its checksum file in one directory:

```sh
sha256sum --check --ignore-missing SHA256SUMS
tar -xzf babel-rs-0.6.0-x86_64-unknown-linux-musl.tar.gz
cd babel-rs-0.6.0-x86_64-unknown-linux-musl
./babel-rs --version
sudo install -m 0755 babel-rs /usr/bin/babel-rs
```

Confirm the selected archive is listed as `OK` before extracting it. Release
assets are available only after the corresponding release workflow succeeds;
build from source if the desired version has no binary bundle.

For a new systemd installation, arrange the host networking described below,
then copy and edit the configuration before starting.
For an upgrade, preserve the existing configuration and state, review the
changelog, and stop the running service before replacing its executable.

```sh
sudo install -m 0600 examples/babel-rs.toml /etc/babel-rs.toml
sudo install -m 0644 packaging/systemd/babel-rs.service /etc/systemd/system/babel-rs.service
sudoedit /etc/babel-rs.toml
sudo babel-rs check --config /etc/babel-rs.toml
sudo systemctl daemon-reload
sudo systemctl enable --now babel-rs
```

Replace example interface names and origin prefixes. The host must provide
interfaces, addresses, routes to local origins, forwarding and firewall policy.
The example's ordinary export table 20000 needs host-owned IPv4/IPv6 lookup
rules before the main table; configure these using the
[host networking guide](https://github.com/bnkrr/babel-rs/blob/main/docs/guide/configuration.md#host-networking-and-export-tables)
before starting the service. The service uses `/usr/bin/babel-rs`, keeps state
in `/var/lib/babel-rs`, and uses `/run/babel-rs/babel-rs.ctl` for local control.

Version 0.6.0 is usable within the
[documented support scope](https://github.com/bnkrr/babel-rs/blob/main/docs/guide/support.md).
Most project code was written by **OpenAI Codex**; tests and RFC review do not
replace an independent security audit. Pin deployed versions, validate changes
on your topology, and retain a rollback path. Breaking public API or behavior
changes use a new 0.x minor version.
