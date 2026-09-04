#!/bin/sh
set -eu

daemon=${1:?usage: netns-rtt.sh /path/to/babel-rs}
test -x "${daemon}" || { echo "daemon is not executable: ${daemon}" >&2; exit 2; }
test "$(id -u)" -eq 0 || { echo "netns E2E must run as root" >&2; exit 2; }
for command in ip grep mktemp kill; do command -v "${command}" >/dev/null || { echo "missing command: ${command}" >&2; exit 2; }; done

suffix=$$
ns_a="vb-rtt-a-${suffix}"
ns_b="vb-rtt-b-${suffix}"
runtime=$(mktemp -d /tmp/babel-rs-rtt.XXXXXXXX)
pid_a=
pid_b=
cleanup() {
  status=$?
  if test "${status}" -ne 0; then
    test ! -f "${runtime}/a.log" || tail -n 120 "${runtime}/a.log" >&2
    test ! -f "${runtime}/b.log" || tail -n 120 "${runtime}/b.log" >&2
    test ! -f "${runtime}/neighbors.json" || cat "${runtime}/neighbors.json" >&2
    ip -n "${ns_a}" -6 route show table all >&2 2>/dev/null || true
    ip -n "${ns_b}" -6 route show table all >&2 2>/dev/null || true
  fi
  test -z "${pid_a}" || kill "${pid_a}" 2>/dev/null || true
  test -z "${pid_b}" || kill "${pid_b}" 2>/dev/null || true
  ip netns del "${ns_a}" 2>/dev/null || true
  ip netns del "${ns_b}" 2>/dev/null || true
  rm -rf -- "${runtime}"
}
trap cleanup EXIT INT TERM

ip netns add "${ns_a}"
ip netns add "${ns_b}"
ip link add "vra-${suffix}" type veth peer name "vrb-${suffix}"
ip link set "vra-${suffix}" netns "${ns_a}" name babel0
ip link set "vrb-${suffix}" netns "${ns_b}" name babel0
for spec in "${ns_a}:fe80::a" "${ns_b}:fe80::b"; do
  ns=${spec%%:*}
  address=${spec#*:}
  ip -n "${ns}" link set lo up
  ip -n "${ns}" link set babel0 addrgenmode none
  ip -n "${ns}" -6 addr add "${address}/64" dev babel0 nodad
  ip -n "${ns}" link set babel0 up
done

write_config() {
  node=$1
  router_id=$2
  table=$3
  origin=$4
  {
    printf 'router_id = "%s"\n' "${router_id}"
    printf 'state_file = "%s/%s.state"\n' "${runtime}" "${node}"
    printf 'interfaces = ["babel0"]\n\n'
    printf '[metric]\ntype = "rtt"\nprobe_interval_ms = 2000\nhalf_life_ms = 6000\nmin_rtt_ms = 10\nmax_rtt_ms = 120\nmax_penalty = 150\n\n'
    printf '[metric.base]\ntype = "wired"\nnominal_cost = 96\nreceived = 2\nwindow = 3\n\n'
    if test -n "${origin}"; then
      printf '[[origins]]\ndestination = "%s"\n\n' "${origin}"
    fi
    printf '[export]\nprotocol = 203\ndevice_only = false\nmanage_rules = true\n\n[[export.views]]\ntable = %s\n' "${table}"
  } >"${runtime}/${node}.toml"
}
write_config a 01:01:01:01:01:01:01:01 23001 ''
write_config b 02:02:02:02:02:02:02:02 23002 2001:db8:9616::/64

ip netns exec "${ns_a}" "${daemon}" run --config "${runtime}/a.toml" --control-socket "${runtime}/a.ctl" >"${runtime}/a.log" 2>&1 &
pid_a=$!
ip netns exec "${ns_b}" "${daemon}" run --config "${runtime}/b.toml" --control-socket "${runtime}/b.ctl" >"${runtime}/b.log" 2>&1 &
pid_b=$!

attempt=0
while :; do
  route=$(ip -n "${ns_a}" -6 route show table 23001 exact 2001:db8:9616::/64 proto 203 2>/dev/null || true)
  if test -S "${runtime}/a.ctl"; then
    ip netns exec "${ns_a}" "${daemon}" neighbors --socket "${runtime}/a.ctl" >"${runtime}/neighbors.json" 2>/dev/null || true
  fi
  if test -n "${route}" \
    && grep -q '"algorithm": "rtt(wired)"' "${runtime}/neighbors.json" 2>/dev/null \
    && grep -Eq '"last_rtt_us": [0-9]+' "${runtime}/neighbors.json" 2>/dev/null \
    && grep -Eq '"smoothed_rtt_us": [0-9]+' "${runtime}/neighbors.json" 2>/dev/null; then
    break
  fi
  attempt=$((attempt + 1))
  test "${attempt}" -lt 45 || { echo "RFC 9616 RTT sampling did not converge" >&2; exit 1; }
  sleep 1
done

echo "babel-rs RFC 9616 timestamp/RTT metric E2E: PASS"
