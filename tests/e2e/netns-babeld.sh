#!/bin/sh
set -eu

daemon=${1:?usage: netns-babeld.sh /path/to/babel-rs}
test -x "${daemon}" || { echo "daemon is not executable: ${daemon}" >&2; exit 2; }
test "$(id -u)" -eq 0 || { echo "netns E2E must run as root" >&2; exit 2; }
for command in ip babeld grep mktemp kill tail; do command -v "${command}" >/dev/null || { echo "missing command: ${command}" >&2; exit 2; }; done

suffix=$$
ns_rs="vb-rs-${suffix}"
ns_c="vb-c-${suffix}"
runtime=$(mktemp -d /tmp/babel-rs-babeld.XXXXXXXX)
pid_rs=
pid_c=
cleanup() {
  status=$?
  if test "${status}" -ne 0; then
    test ! -f "${runtime}/rs.log" || tail -n 160 "${runtime}/rs.log" >&2
    test ! -f "${runtime}/babeld.log" || tail -n 80 "${runtime}/babeld.log" >&2
    ip -n "${ns_rs}" -6 -details rule show >&2 2>/dev/null || true
    ip -n "${ns_rs}" -6 route show table all >&2 2>/dev/null || true
    ip -n "${ns_rs}" route show table all >&2 2>/dev/null || true
    ip -n "${ns_c}" -6 route show table all >&2 2>/dev/null || true
    ip -n "${ns_c}" route show table all >&2 2>/dev/null || true
  fi
  test -z "${pid_rs}" || kill "${pid_rs}" 2>/dev/null || true
  test -z "${pid_c}" || kill "${pid_c}" 2>/dev/null || true
  ip netns del "${ns_rs}" 2>/dev/null || true
  ip netns del "${ns_c}" 2>/dev/null || true
  rm -rf -- "${runtime}"
}
trap cleanup EXIT INT TERM

ip netns add "${ns_rs}"
ip netns add "${ns_c}"
ip link add "vr-${suffix}" type veth peer name "vc-${suffix}"
ip link set "vr-${suffix}" netns "${ns_rs}" name babel0
ip link set "vc-${suffix}" netns "${ns_c}" name babel0
ip -n "${ns_rs}" link set lo up
ip -n "${ns_c}" link set lo up
ip -n "${ns_rs}" link set babel0 addrgenmode none
ip -n "${ns_c}" link set babel0 addrgenmode none
ip -n "${ns_rs}" -6 addr add fe80::1/64 dev babel0 nodad
ip -n "${ns_c}" -6 addr add fe80::2/64 dev babel0 nodad
ip -n "${ns_rs}" link set babel0 up
ip -n "${ns_c}" link set babel0 up
ip -n "${ns_c}" -6 route add blackhole 2001:db8:200::/64 proto 99
ip -n "${ns_rs}" -6 route add 2001:db8:dead::/64 dev babel0 table 20000 proto 203

cat >"${runtime}/rs.toml" <<EOF
state_file = "${runtime}/rs.router-id"
interfaces = ["babel0"]

[[origins]]
destination = "2001:db8:100::/64"

[[origins]]
destination = "198.51.100.0/24"

[[origins]]
destination = "2001:db8:300::/64"
source = "2001:db8:aaaa::/64"

[export]
protocol = 203
device_only = false
manage_rules = true

[[export.views]]
table = 20000

[[export.views]]
table = 20001
source = "2001:db8:aaaa::/64"
EOF

cat >"${runtime}/babeld.conf" <<EOF
ipv6-subtrees true
redistribute proto 99 allow
redistribute local deny
EOF

ip netns exec "${ns_rs}" env RUST_LOG=debug "${daemon}" --config "${runtime}/rs.toml" >"${runtime}/rs.log" 2>&1 &
pid_rs=$!
start_babeld() {
  ip netns exec "${ns_c}" babeld -d 1 -L "${runtime}/babeld.log" \
    -I "${runtime}/babeld.pid" -S "${runtime}/babeld.state" \
    -t 201 -c "${runtime}/babeld.conf" babel0 &
  pid_c=$!
}
start_babeld

attempt=0
while :; do
  rs_v6=$(ip -n "${ns_rs}" -6 route show table 20000 exact 2001:db8:200::/64 proto 203 2>/dev/null || true)
  c_v6=$(ip -n "${ns_c}" -6 route show table 201 exact 2001:db8:100::/64 2>/dev/null || true)
  c_v4=$(ip -n "${ns_c}" route show table 201 exact 198.51.100.0/24 2>/dev/null || true)
  c_ss=$(ip -n "${ns_c}" -6 route show table 201 from 2001:db8:aaaa::/64 exact 2001:db8:300::/64 2>/dev/null || true)
  rs_rule=$(ip -n "${ns_rs}" -6 -details rule show 2>/dev/null | grep 'from 2001:db8:aaaa::/64 lookup 20001 proto 203' || true)
  stale=$(ip -n "${ns_rs}" -6 route show table 20000 exact 2001:db8:dead::/64 proto 203 2>/dev/null || true)
  if test -n "${rs_v6}" && test -n "${c_v6}" && test -n "${c_v4}" && test -n "${c_ss}" && test -n "${rs_rule}" && test -z "${stale}"; then
    break
  fi
  attempt=$((attempt + 1))
  if test "${attempt}" -ge 40; then
    echo "babeld interoperability did not converge" >&2
    exit 1
  fi
  sleep 1
done

# Restarting the peer forces all supported babeld versions to rescan the
# kernel RIB.  A removed redistributed route must retract, and a later
# announcement must be acquired again with the same source/route machinery.
ip -n "${ns_c}" -6 route del blackhole 2001:db8:200::/64 proto 99
kill -TERM "${pid_c}"
wait "${pid_c}" || true
pid_c=
start_babeld
attempt=0
while ip -n "${ns_rs}" -6 route show table 20000 exact 2001:db8:200::/64 proto 203 2>/dev/null | grep -q .; do
  attempt=$((attempt + 1))
  test "${attempt}" -lt 45 || { echo "babeld retraction did not converge" >&2; exit 1; }
  sleep 1
done
kill -TERM "${pid_c}"
wait "${pid_c}" || true
pid_c=
ip -n "${ns_c}" -6 route add blackhole 2001:db8:200::/64 proto 99
start_babeld
attempt=0
while ! ip -n "${ns_rs}" -6 route show table 20000 exact 2001:db8:200::/64 proto 203 2>/dev/null | grep -q .; do
  attempt=$((attempt + 1))
  test "${attempt}" -lt 45 || { echo "babeld re-announcement did not converge" >&2; exit 1; }
  sleep 1
done

# SIGTERM is graceful: owned routes are removed. Restart preserves Router-ID and
# advances the persisted sequence number before advertising again.
router_before=$(awk -F'"' '/^router_id/ { print $2 }' "${runtime}/rs.router-id")
seq_before=$(awk '/^sequence_number/ { print $3 }' "${runtime}/rs.router-id")
kill -TERM "${pid_rs}"
wait "${pid_rs}" || true
pid_rs=
attempt=0
while ip -n "${ns_rs}" -6 route show table 20000 proto 203 2>/dev/null | grep -q .; do
  attempt=$((attempt + 1))
  test "${attempt}" -lt 10 || { echo "graceful shutdown left owned routes" >&2; exit 1; }
  sleep 1
done
test -z "$(ip -n "${ns_rs}" -6 -details rule show | grep 'proto 203' || true)"
ip netns exec "${ns_rs}" env RUST_LOG=debug "${daemon}" --config "${runtime}/rs.toml" >"${runtime}/rs.log" 2>&1 &
pid_rs=$!
attempt=0
while ! ip -n "${ns_rs}" -6 route show table 20000 exact 2001:db8:200::/64 proto 203 2>/dev/null | grep -q .; do
  attempt=$((attempt + 1))
  test "${attempt}" -lt 45 || { echo "restart did not converge" >&2; exit 1; }
  sleep 1
done
router_after=$(awk -F'"' '/^router_id/ { print $2 }' "${runtime}/rs.router-id")
seq_after=$(awk '/^sequence_number/ { print $3 }' "${runtime}/rs.router-id")
test "${router_before}" = "${router_after}" || { echo "Router-ID changed across restart" >&2; exit 1; }
test "${seq_before}" != "${seq_after}" || { echo "sequence number did not advance across restart" >&2; exit 1; }

echo "babel-rs <-> babeld exchange/retract/restart E2E: PASS"
