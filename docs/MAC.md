# Babel MAC authentication

RFC 8967 authentication is optional per interface. Configuring keys enables
strict reception from the first packet; omitting MAC configuration preserves
plain Babel. Authentication protects routing messages, not their confidentiality.
DTLS remains unimplemented.

Generate a random 32-byte key, keep its file readable only by the daemon account,
and distribute it to the authorized peers on the link:

```sh
umask 077
openssl rand -hex 32 > /etc/babel-rs/link.key
```

Attach this to the relevant `[[interfaces]]` section:

```toml
[interfaces.mac]
[[interfaces.mac.keys]]
algorithm = "hmac_sha256"
key_file = "/etc/babel-rs/link.key"
```

`hmac_sha256` (default) and `blake2s128` are supported. Key files contain hex,
with optional surrounding whitespace. HMAC accepts 1–4096 key bytes; BLAKE2s
accepts 1–32. Random 32-byte keys are recommended for either. Each interface
accepts 1–8 keys. All configured keys sign outgoing packets; any matching key
can authenticate incoming packets. Key values are omitted from Debug/status.

For rotation, add a second `[[interfaces.mac.keys]]`, reload all peers, then
remove the old entry and reload again. `reload`/SIGHUP rereads key files,
including changes at the same path. A changed key set detaches and reattaches
the affected interface, discarding old adjacency and queued input/output before
a fresh challenge. The process stays running; expect brief route reconvergence.
Unreadable or malformed keys reject the candidate configuration.

RFC 8967 migration mode is an explicit `accept_unverified = true` in
`[interfaces.mac]`. It signs output but accepts unverified input and therefore
does not authenticate peers. The default is false; there is no automatic
fallback. Interface status reports `mac_mode` as `disabled`, `strict`, or
`migration`, and `udp_payload_budget` excludes authentication overhead.

The runtime obtains real source/destination addresses with Linux packet-info
metadata, pins the signed transmit source with `sendmsg`, and authenticates UDP
ports as well as the Babel header/body. It stamps RTT timestamps before signing
and reserves one PC TLV plus all MAC trailers before packetization. Invalid
MACs cannot allocate authentication peer state or reach the routing engine.

Each interface has a CSPRNG-generated 128-bit Index and increasing 32-bit PC.
Restart/counter rollover generates a fresh Index. Challenges use fresh 128-bit
nonces, expire after 30 seconds, and are limited to one request per interface
per 300 ms and one reply per peer per 300 ms. Authentication peer state is
bounded by `max_neighbors` per interface and expires five minutes after the
last accepted packet; failed challenges do not extend it. RFC 9467 §3.1 splits
received unicast and multicast counters. Its optional replay window is not used.

Embedders construct `MacKey`/`MacConfig` and use
`BabelRouterBuilder::interface_with_mac` or
`RouterHandle::add_interface_with_mac`. Remove/reattach an interface to replace
its keys. Sans-I/O hosts use `babel_protocol::mac::MacSession`: supply CSPRNG
bytes, monotonic time, actual UDP endpoints, and promptly transmit returned
challenge controls. Never clone/reuse a session's outgoing Index/PC state.

Evidence lives in `crates/babel-protocol/src/mac/tests.rs` and
`tests/e2e/netns-mac-sadr.py`: independent digest vectors, tamper/replay rejection,
challenge expiry/rate limits, counter rollover, restart, rotation, malformed
framing, both IP control families, and independent babeld authentication.
