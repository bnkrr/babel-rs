#!/bin/sh
set -eu

daemon=${1:?usage: netns-bird.sh /path/to/babel-rs}
test -x "${daemon}" || { echo "daemon is not executable: ${daemon}" >&2; exit 2; }
test "$(id -u)" -eq 0 || { echo "netns E2E must run as root" >&2; exit 2; }
for command in ip bird grep mktemp kill; do command -v "${command}" >/dev/null || { echo "missing command: ${command}" >&2; exit 2; }; done

suffix=$$
ns_rs="vbird-rs-${suffix}"
ns_c="vbird-c-${suffix}"
runtime=$(mktemp -d /tmp/babel-rs-bird.XXXXXXXX)
pid_rs=
pid_c=
cleanup() {
  status=$?
  if test "${status}" -ne 0; then
    test ! -f "${runtime}/rs.log" || cat "${runtime}/rs.log" >&2
    test ! -f "${runtime}/bird.log" || cat "${runtime}/bird.log" >&2
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

cat >"${runtime}/rs.toml" <<EOF
router_id = "11:12:13:14:15:16:17:18"
state_file = "${runtime}/rs.router-id"
interfaces = ["babel0"]

[[origins]]
destination = "2001:db8:510::/64"

[[origins]]
destination = "198.51.100.0/24"

[[origins]]
destination = "2001:db8:530::/64"
source = "2001:db8:aaaa::/64"

[export]
protocol = 204
device_only = false
manage_rules = true

[[export.views]]
table = 21000

[[export.views]]
table = 21001
source = "2001:db8:aaaa::/64"
EOF

cat >"${runtime}/bird.conf" <<EOF
log "${runtime}/bird.log" all;
router id 192.0.2.2;

ipv4 table babel4;
ipv6 sadr table babel6;

protocol device { }

protocol static origin4 {
  ipv4 { table babel4; };
  route 203.0.113.0/24 blackhole;
}

protocol static origin6 {
  ipv6 sadr { table babel6; };
  route 2001:db8:520::/64 from ::/0 blackhole;
}

protocol babel babel_rs_interop {
  ipv4 { table babel4; import all; export all; };
  ipv6 sadr { table babel6; import all; export all; };
  interface "babel0" {
    type wired;
    hello interval 1s;
    update interval 4s;
  };
}

protocol kernel export4 {
  ipv4 { table babel4; import none; export all; };
  kernel table 202;
}

protocol kernel export6 {
  ipv6 sadr { table babel6; import none; export all; };
  kernel table 202;
}
EOF

ip netns exec "${ns_c}" bird -p -c "${runtime}/bird.conf"
ip netns exec "${ns_rs}" env RUST_LOG=debug "${daemon}" --config "${runtime}/rs.toml" >"${runtime}/rs.log" 2>&1 &
pid_rs=$!
ip netns exec "${ns_c}" bird -f -c "${runtime}/bird.conf" -P "${runtime}/bird.pid" -s "${runtime}/bird.ctl" &
pid_c=$!

attempt=0
while :; do
  rs_v6=$(ip -n "${ns_rs}" -6 route show table 21000 exact 2001:db8:520::/64 proto 204 2>/dev/null || true)
  rs_v4=$(ip -n "${ns_rs}" route show table 21000 exact 203.0.113.0/24 proto 204 2>/dev/null || true)
  c_v6=$(ip -n "${ns_c}" -6 route show table 202 exact 2001:db8:510::/64 2>/dev/null || true)
  c_v4=$(ip -n "${ns_c}" route show table 202 exact 198.51.100.0/24 2>/dev/null || true)
  c_ss=$(ip -n "${ns_c}" -6 route show table 202 from 2001:db8:aaaa::/64 exact 2001:db8:530::/64 2>/dev/null || true)
  rs_rule=$(ip -n "${ns_rs}" -6 -details rule show 2>/dev/null | grep 'from 2001:db8:aaaa::/64 lookup 21001 proto 204' || true)
  if test -n "${rs_v6}" && test -n "${rs_v4}" && test -n "${c_v6}" && test -n "${c_v4}" && test -n "${c_ss}" && test -n "${rs_rule}"; then
    break
  fi
  attempt=$((attempt + 1))
  if test "${attempt}" -ge 45; then
    echo "BIRD interoperability did not converge" >&2
    exit 1
  fi
  sleep 1
done

echo "babel-rs <-> BIRD RFC 8966/9079/9229 E2E: PASS"
