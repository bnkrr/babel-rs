#!/bin/sh
set -eu

daemon=${1:?usage: netns-lifecycle.sh /path/to/babel-rs}
test -x "${daemon}" || { echo "daemon is not executable: ${daemon}" >&2; exit 2; }
test "$(id -u)" -eq 0 || { echo "netns E2E must run as root" >&2; exit 2; }
for command in ip babeld grep mktemp kill mv cp; do command -v "${command}" >/dev/null || { echo "missing command: ${command}" >&2; exit 2; }; done

suffix=$$
ns_rs="vbl-rs-${suffix}"
ns_peer="vbl-p-${suffix}"
runtime=$(mktemp -d /tmp/babel-rs-lifecycle.XXXXXXXX)
control_socket="${runtime}/babel-rs.ctl"
pid_rs=
pid_peer=
cleanup() {
  status=$?
  if test "${status}" -ne 0; then
    test ! -f "${runtime}/rs.log" || tail -n 160 "${runtime}/rs.log" >&2
    test ! -f "${runtime}/peer.log" || tail -n 80 "${runtime}/peer.log" >&2
    ip -n "${ns_rs}" -6 route show table all >&2 2>/dev/null || true
    ip -n "${ns_peer}" -6 route show table all >&2 2>/dev/null || true
  fi
  test -z "${pid_rs}" || kill "${pid_rs}" 2>/dev/null || true
  test -z "${pid_peer}" || kill "${pid_peer}" 2>/dev/null || true
  ip netns del "${ns_rs}" 2>/dev/null || true
  ip netns del "${ns_peer}" 2>/dev/null || true
  rm -rf -- "${runtime}"
}
trap cleanup EXIT INT TERM

write_config() {
  path=$1
  origin=${2:-}
  table=${3:-23201}
  {
    printf 'router_id = "11:12:13:14:15:16:17:18"\n'
    printf 'state_file = "%s/rs.state"\n' "${runtime}"
    printf 'interfaces = ["babel?"]\n\n'
    if test -n "${origin}"; then
      printf '[[origins]]\ndestination = "%s"\n\n' "${origin}"
    fi
    printf '[export]\nprotocol = 203\ndevice_only = false\nmanage_rules = false\n\n'
    printf '[[export.views]]\ntable = %s\n' "${table}"
  } >"${path}"
}

create_link() {
  ip link add "vlr-${suffix}" type veth peer name "vlp-${suffix}"
  ip link set "vlr-${suffix}" netns "${ns_rs}" name babel0
  ip link set "vlp-${suffix}" netns "${ns_peer}" name babel0
  ip -n "${ns_rs}" link set babel0 addrgenmode none
  ip -n "${ns_peer}" link set babel0 addrgenmode none
  ip -n "${ns_rs}" -6 addr add fe80::1/64 dev babel0 nodad
  ip -n "${ns_peer}" -6 addr add fe80::2/64 dev babel0 nodad
  ip -n "${ns_rs}" link set babel0 up
  ip -n "${ns_peer}" link set babel0 up
}

start_peer() {
  ip netns exec "${ns_peer}" babeld -d 1 -L "${runtime}/peer.log" \
    -I "${runtime}/peer.pid" -S "${runtime}/peer.state" -t 201 \
    -c "${runtime}/peer.conf" babel0 >"${runtime}/peer.stdout" 2>&1 &
  pid_peer=$!
}

wait_route() {
  ns=$1 table=$2 prefix=$3 present=$4 limit=$5 attempt=0
  while :; do
    route=$(ip -n "${ns}" -6 route show table "${table}" exact "${prefix}" 2>/dev/null || true)
    if { test "${present}" = yes && test -n "${route}"; } || { test "${present}" = no && test -z "${route}"; }; then return; fi
    attempt=$((attempt + 1))
    test "${attempt}" -lt "${limit}" || { echo "route ${prefix} presence=${present} did not converge in ${ns}" >&2; exit 1; }
    sleep 1
  done
}

prefix_initial=2001:db8:710::/64
prefix_rebound=2001:db8:720::/64
prefix_reload=2001:db8:730::/64

ip netns add "${ns_rs}"
ip netns add "${ns_peer}"
ip -n "${ns_rs}" link set lo up
ip -n "${ns_peer}" link set lo up
write_config "${runtime}/rs.toml"
cat >"${runtime}/peer.conf" <<EOF
ipv6-subtrees true
redistribute proto 99 allow
redistribute local deny
EOF

# The desired glob is valid even when it initially matches no interface.
ip netns exec "${ns_rs}" env RUST_LOG=debug "${daemon}" run --config "${runtime}/rs.toml" --control-socket "${control_socket}" >"${runtime}/rs.log" 2>&1 &
pid_rs=$!
attempt=0
until ip netns exec "${ns_rs}" "${daemon}" status --socket "${control_socket}" >"${runtime}/status.json" 2>/dev/null; do
  attempt=$((attempt + 1)); test "${attempt}" -lt 30 || { echo "control socket did not become ready" >&2; exit 1; }; sleep 1
done
grep -q '"ready": true' "${runtime}/status.json"

create_link
ip -n "${ns_peer}" -6 route replace blackhole "${prefix_initial}" proto 99
start_peer
wait_route "${ns_rs}" 23201 "${prefix_initial}" yes 35

# A complete valid candidate adds an origin without restarting the daemon.
write_config "${runtime}/rs.toml.new" "${prefix_reload}" 23202
mv "${runtime}/rs.toml.new" "${runtime}/rs.toml"
ip netns exec "${ns_rs}" "${daemon}" reload --socket "${control_socket}" >"${runtime}/reload.json"
grep -q 'active_config_sha256' "${runtime}/reload.json"
wait_route "${ns_peer}" 201 "${prefix_reload}" yes 20
wait_route "${ns_rs}" 23202 "${prefix_initial}" yes 8
wait_route "${ns_rs}" 23201 "${prefix_initial}" no 8

# A malformed candidate is rejected and the committed origin remains active.
cp "${runtime}/rs.toml" "${runtime}/rs.toml.valid"
printf 'this is not TOML = [\n' >"${runtime}/rs.toml.new"
mv "${runtime}/rs.toml.new" "${runtime}/rs.toml"
kill -HUP "${pid_rs}"
sleep 2
kill -0 "${pid_rs}"
wait_route "${ns_peer}" 201 "${prefix_reload}" yes 5
grep -q 'configuration reload rejected' "${runtime}/rs.log"
ip netns exec "${ns_rs}" "${daemon}" status --socket "${control_socket}" | grep -q 'last_reload_error'
cp "${runtime}/rs.toml.valid" "${runtime}/rs.toml.new"
mv "${runtime}/rs.toml.new" "${runtime}/rs.toml"

# Replacing an interface by the same name creates a new ifindex.  A prefix
# introduced only after replacement proves that the new socket is active.
ip -n "${ns_rs}" link del babel0
wait_route "${ns_rs}" 23202 "${prefix_initial}" no 10
kill "${pid_peer}" 2>/dev/null || true
wait "${pid_peer}" 2>/dev/null || true
pid_peer=
create_link
ip -n "${ns_peer}" -6 route replace blackhole "${prefix_initial}" proto 99
ip -n "${ns_peer}" -6 route replace blackhole "${prefix_rebound}" proto 99
start_peer
wait_route "${ns_rs}" 23202 "${prefix_rebound}" yes 35

ip netns exec "${ns_rs}" "${daemon}" interfaces --socket "${control_socket}" | grep -q 'babel0'
ip netns exec "${ns_rs}" "${daemon}" neighbors --socket "${control_socket}" | grep -q 'fe80::2'
ip netns exec "${ns_rs}" "${daemon}" routes --socket "${control_socket}" --interface babel0 | grep -q "${prefix_initial%/64}"

# The periodic full-snapshot reconciler repairs out-of-band FIB deletion.
ip -n "${ns_rs}" -6 route del table 23202 "${prefix_initial}"
wait_route "${ns_rs}" 23202 "${prefix_initial}" yes 8

# The command acknowledges before the shared graceful shutdown path begins.
ip netns exec "${ns_rs}" "${daemon}" shutdown --socket "${control_socket}" | grep -q 'accepted'
wait "${pid_rs}"
pid_rs=

echo "babel-rs control/interface/rebind/reload/FIB reconciliation E2E: PASS"
