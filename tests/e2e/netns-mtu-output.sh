#!/bin/sh
set -eu

daemon=${1:?usage: netns-mtu-output.sh /path/to/babel-rs}
test -x "${daemon}" || { echo "daemon is not executable: ${daemon}" >&2; exit 2; }
test "$(id -u)" -eq 0 || { echo "netns E2E must run as root" >&2; exit 2; }
for command in ip grep mktemp kill wc; do command -v "${command}" >/dev/null || { echo "missing command: ${command}" >&2; exit 2; }; done

suffix=$$
ns_a="vb-mtu-a-${suffix}"
ns_b="vb-mtu-b-${suffix}"
runtime=$(mktemp -d /tmp/babel-rs-mtu.XXXXXXXX)
pid_a=
pid_b=
cleanup() {
  status=$?
  if test "${status}" -ne 0; then
    test ! -f "${runtime}/a.log" || tail -n 100 "${runtime}/a.log" >&2
    test ! -f "${runtime}/b.log" || tail -n 100 "${runtime}/b.log" >&2
    test ! -f "${runtime}/interfaces.json" || cat "${runtime}/interfaces.json" >&2
    ip -n "${ns_a}" -6 route show table 25001 >&2 2>/dev/null || true
  fi
  test -z "${pid_a}" || kill "${pid_a}" 2>/dev/null || true
  test -z "${pid_b}" || kill "${pid_b}" 2>/dev/null || true
  test -z "${pid_a}" || wait "${pid_a}" 2>/dev/null || true
  test -z "${pid_b}" || wait "${pid_b}" 2>/dev/null || true
  ip netns del "${ns_a}" 2>/dev/null || true
  ip netns del "${ns_b}" 2>/dev/null || true
  rm -rf -- "${runtime}"
}
trap cleanup EXIT INT TERM

ip netns add "${ns_a}"
ip netns add "${ns_b}"
ip link add "vma-${suffix}" type veth peer name "vmb-${suffix}"
ip link set "vma-${suffix}" netns "${ns_a}" name babel0
ip link set "vmb-${suffix}" netns "${ns_b}" name babel0
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
  origins=$4
  {
    printf 'router_id = "%s"\n' "${router_id}"
    printf 'state_file = "%s/%s.state"\n' "${runtime}" "${node}"
    printf 'interfaces = ["babel0"]\n\n'
    sequence=1
    while test "${sequence}" -le "${origins}"; do
      printf '[[origins]]\ndestination = "2001:db8:25:%x::1/128"\n\n' "${sequence}"
      sequence=$((sequence + 1))
    done
    printf '[export]\nprotocol = 203\ndevice_only = false\nmanage_rules = true\n\n[[export.views]]\ntable = %s\n' "${table}"
  } >"${runtime}/${node}.toml"
}
write_config a 01:01:01:01:01:01:01:01 25001 0
write_config b 02:02:02:02:02:02:02:02 25002 120

ip netns exec "${ns_a}" "${daemon}" run --config "${runtime}/a.toml" --control-socket "${runtime}/a.ctl" >"${runtime}/a.log" 2>&1 &
pid_a=$!
ip netns exec "${ns_b}" "${daemon}" run --config "${runtime}/b.toml" --control-socket "${runtime}/b.ctl" >"${runtime}/b.log" 2>&1 &
pid_b=$!

wait_route_count() {
  expected=$1
  attempt=0
  while :; do
    count=$(ip -n "${ns_a}" -6 route show table 25001 proto 203 2>/dev/null | wc -l)
    test "${count}" -eq "${expected}" && return
    attempt=$((attempt + 1))
    test "${attempt}" -lt 60 || { echo "expected ${expected} routes after MTU packetisation, got ${count}" >&2; exit 1; }
    sleep 1
  done
}
wait_route_count 120

# MTU is live interface state.  Lower it without restarting either process,
# then add enough origins for several Babel datagrams and reload atomically.
ip -n "${ns_a}" link set babel0 mtu 1280
ip -n "${ns_b}" link set babel0 mtu 1280
write_config b 02:02:02:02:02:02:02:02 25002 240
ip netns exec "${ns_b}" "${daemon}" reload --socket "${runtime}/b.ctl" >/dev/null
wait_route_count 240

ip netns exec "${ns_b}" "${daemon}" interfaces --socket "${runtime}/b.ctl" >"${runtime}/interfaces.json"
grep -q '"mtu": 1280' "${runtime}/interfaces.json"
grep -q '"udp_payload_budget": 1232' "${runtime}/interfaces.json"

echo "babel-rs live-MTU packetisation/output scheduler E2E: PASS"
