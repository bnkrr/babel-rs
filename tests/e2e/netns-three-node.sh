#!/bin/sh
set -eu

daemon=${1:?usage: netns-three-node.sh /path/to/babel-rs}
test -x "${daemon}" || { echo "daemon is not executable: ${daemon}" >&2; exit 2; }
test "$(id -u)" -eq 0 || { echo "netns E2E must run as root" >&2; exit 2; }
for command in ip mktemp kill grep; do command -v "${command}" >/dev/null || { echo "missing command: ${command}" >&2; exit 2; }; done

suffix=$$
ns_a="vb-a-${suffix}"
ns_b="vb-b-${suffix}"
ns_c="vb-c-${suffix}"
runtime=$(mktemp -d /tmp/babel-rs-three.XXXXXXXX)
pid_a=
pid_b=
pid_c=
cleanup() {
  status=$?
  if test "${status}" -ne 0; then
    for node in a b c; do
      test ! -f "${runtime}/${node}.log" || tail -n 80 "${runtime}/${node}.log" >&2
    done
    for ns in "${ns_a}" "${ns_b}" "${ns_c}"; do
      ip -n "${ns}" -6 route show table all >&2 2>/dev/null || true
    done
  fi
  test -z "${pid_a}" || kill "${pid_a}" 2>/dev/null || true
  test -z "${pid_b}" || kill "${pid_b}" 2>/dev/null || true
  test -z "${pid_c}" || kill "${pid_c}" 2>/dev/null || true
  ip netns del "${ns_a}" 2>/dev/null || true
  ip netns del "${ns_b}" 2>/dev/null || true
  ip netns del "${ns_c}" 2>/dev/null || true
  rm -rf -- "${runtime}"
}
trap cleanup EXIT INT TERM

for ns in "${ns_a}" "${ns_b}" "${ns_c}"; do
  ip netns add "${ns}"
  ip -n "${ns}" link set lo up
done

ip link add "va-${suffix}" type veth peer name "vb0-${suffix}"
ip link add "vb1-${suffix}" type veth peer name "vc-${suffix}"
ip link set "va-${suffix}" netns "${ns_a}" name babel0
ip link set "vb0-${suffix}" netns "${ns_b}" name babel0
ip link set "vb1-${suffix}" netns "${ns_b}" name babel1
ip link set "vc-${suffix}" netns "${ns_c}" name babel0

for spec in "${ns_a}:babel0:fe80::a1" "${ns_b}:babel0:fe80::b1" "${ns_b}:babel1:fe80::b2" "${ns_c}:babel0:fe80::c1"; do
  ns=${spec%%:*}; rest=${spec#*:}; interface=${rest%%:*}; address=${rest#*:}
  ip -n "${ns}" link set "${interface}" addrgenmode none
  ip -n "${ns}" -6 addr add "${address}/64" dev "${interface}" nodad
  ip -n "${ns}" link set "${interface}" up
done

for expected in "${ns_a}:babel0:fe80::a1" "${ns_b}:babel0:fe80::b1" "${ns_b}:babel1:fe80::b2" "${ns_c}:babel0:fe80::c1"; do
  ns=${expected%%:*}; rest=${expected#*:}; interface=${rest%%:*}; address=${rest#*:}
  ip -n "${ns}" -6 addr show dev "${interface}" | grep -q "${address}/64" || {
    echo "missing test link-local ${address} on ${ns}/${interface}" >&2
    exit 1
  }
done

write_config() {
  node=$1
  router_id=$2
  interfaces=$3
  origin=$4
  table=$5
  {
    printf 'router_id = "%s"\n' "${router_id}"
    printf 'state_file = "%s/%s.state"\n' "${runtime}" "${node}"
    printf 'interfaces = [%s]\n\n' "${interfaces}"
    if test -n "${origin}"; then
      printf '[[origins]]\ndestination = "%s"\n\n' "${origin}"
    fi
    printf '[export]\nprotocol = 203\ndevice_only = false\nmanage_rules = true\n\n[[export.views]]\ntable = %s\n' "${table}"
  } >"${runtime}/${node}.toml"
}
write_config a 01:01:01:01:01:01:01:01 '"babel0"' 2001:db8:a::/64 22001
write_config b 02:02:02:02:02:02:02:02 '"babel0", "babel1"' '' 22002
write_config c 03:03:03:03:03:03:03:03 '"babel0"' 2001:db8:c::/64 22003

ip netns exec "${ns_a}" "${daemon}" --config "${runtime}/a.toml" >"${runtime}/a.log" 2>&1 & pid_a=$!
ip netns exec "${ns_b}" "${daemon}" --config "${runtime}/b.toml" >"${runtime}/b.log" 2>&1 & pid_b=$!
ip netns exec "${ns_c}" "${daemon}" --config "${runtime}/c.toml" >"${runtime}/c.log" 2>&1 & pid_c=$!

wait_route() {
  ns=$1 table=$2 prefix=$3 present=$4 limit=$5 attempt=0
  while :; do
    route=$(ip -n "${ns}" -6 route show table "${table}" exact "${prefix}" proto 203 2>/dev/null || true)
    if { test "${present}" = yes && test -n "${route}"; } || { test "${present}" = no && test -z "${route}"; }; then return; fi
    attempt=$((attempt + 1))
    test "${attempt}" -lt "${limit}" || { echo "route ${prefix} presence=${present} did not converge in ${ns}" >&2; exit 1; }
    sleep 1
  done
}

wait_route "${ns_a}" 22001 2001:db8:c::/64 yes 45
wait_route "${ns_c}" 22003 2001:db8:a::/64 yes 45

# Break the B-C adjacency. B must emit a triggered retraction towards A rather
# than leaving the two-hop route installed until its advertised hold time.
ip -n "${ns_b}" link set babel1 down
ip -n "${ns_c}" link set babel0 down
wait_route "${ns_a}" 22001 2001:db8:c::/64 no 25

ip -n "${ns_b}" link set babel1 up
ip -n "${ns_c}" link set babel0 up
ip -n "${ns_b}" -6 addr replace fe80::b2/64 dev babel1 nodad
ip -n "${ns_c}" -6 addr replace fe80::c1/64 dev babel0 nodad
wait_route "${ns_a}" 22001 2001:db8:c::/64 yes 45

echo "babel-rs three-node propagation/link-failure E2E: PASS"
