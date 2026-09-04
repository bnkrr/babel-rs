#!/usr/bin/env bash
set -euo pipefail

daemon=${1:?usage: netns-rtt-multipath.sh /path/to/babel-rs}
test -x "${daemon}" || { echo "daemon is not executable: ${daemon}" >&2; exit 2; }
test "$(id -u)" -eq 0 || { echo "netns E2E must run as root" >&2; exit 2; }
for command in ip tc ping sysctl grep mktemp kill sleep; do
  command -v "${command}" >/dev/null || { echo "missing command: ${command}" >&2; exit 2; }
done

runtime=$(mktemp -d /tmp/babel-rs-rtt-multipath.XXXXXXXX)
suffix=${runtime##*.}
nodes=(a b c d e f)
pids=()
ns() { printf 'vbm-%s-%s' "$1" "${suffix}"; }

cleanup() {
  status=$?
  if test "${status}" -ne 0; then
    for node in "${nodes[@]}"; do
      test ! -f "${runtime}/${node}.log" || { echo "--- ${node}.log" >&2; tail -n 80 "${runtime}/${node}.log" >&2; }
    done
    test ! -f "${runtime}/route.json" || cat "${runtime}/route.json" >&2
  fi
  for pid in "${pids[@]}"; do kill "${pid}" 2>/dev/null || true; done
  wait 2>/dev/null || true
  for node in "${nodes[@]}"; do ip netns del "$(ns "${node}")" 2>/dev/null || true; done
  rm -rf -- "${runtime}"
}
trap cleanup EXIT INT TERM

for node in "${nodes[@]}"; do
  ip netns add "$(ns "${node}")"
  ip -n "$(ns "${node}")" link set lo up
  ip netns exec "$(ns "${node}")" sysctl -q -w net.ipv6.conf.all.forwarding=1
done

link_index=0
add_link() {
  left_node=$1 left_if=$2 left_addr=$3 right_node=$4 right_if=$5 right_addr=$6
  left_tmp="m${link_index}l${suffix}"
  right_tmp="m${link_index}r${suffix}"
  link_index=$((link_index + 1))
  ip link add "${left_tmp}" type veth peer name "${right_tmp}"
  ip link set "${left_tmp}" netns "$(ns "${left_node}")"
  ip link set "${right_tmp}" netns "$(ns "${right_node}")"
  ip -n "$(ns "${left_node}")" link set "${left_tmp}" name "${left_if}"
  ip -n "$(ns "${right_node}")" link set "${right_tmp}" name "${right_if}"
  for endpoint in "${left_node}:${left_if}:${left_addr}" "${right_node}:${right_if}:${right_addr}"; do
    node=${endpoint%%:*}; rest=${endpoint#*:}; interface=${rest%%:*}; address=${rest#*:}
    ip -n "$(ns "${node}")" link set "${interface}" addrgenmode none
    ip -n "$(ns "${node}")" -6 addr add "${address}/64" dev "${interface}" nodad
    ip -n "$(ns "${node}")" link set "${interface}" up
  done
}

# Two equal-hop paths plus a three-hop fallback.
add_link a ab fe80:1::1 b ba fe80:1::2
add_link b bd fe80:2::1 d db fe80:2::2
add_link a ac fe80:3::1 c ca fe80:3::2
add_link c cd fe80:4::1 d dc fe80:4::2
add_link a ae fe80:5::1 e ea fe80:5::2
add_link e ef fe80:6::1 f fe fe80:6::2
add_link f fd fe80:7::1 d df fe80:7::2

ip -n "$(ns a)" -6 addr add 2001:db8:a::1/128 dev lo
ip -n "$(ns d)" -6 addr add 2001:db8:d::1/128 dev lo

set_link_delay() {
  ip netns exec "$(ns "$1")" tc qdisc replace dev "$2" root netem delay "$5"ms
  ip netns exec "$(ns "$3")" tc qdisc replace dev "$4" root netem delay "$5"ms
}
set_b_delay() { set_link_delay a ab b ba "$1"; set_link_delay b bd d db "$1"; }
set_c_delay() { set_link_delay a ac c ca "$1"; set_link_delay c cd d dc "$1"; }

set_b_delay 35
set_c_delay 2
set_link_delay a ae e ea 5
set_link_delay e ef f fe 5
set_link_delay f fd d df 5

write_config() {
  node=$1 router_id=$2 interfaces=$3 table=$4 origin=${5:-}
  {
    printf 'router_id = "%s"\n' "${router_id}"
    printf 'state_file = "%s/%s.state"\n' "${runtime}" "${node}"
    printf 'interfaces = %s\n\n' "${interfaces}"
    printf '[metric]\ntype = "rtt"\nprobe_interval_ms = 1000\nhalf_life_ms = 3000\nmin_rtt_ms = 10\nmax_rtt_ms = 120\nmax_penalty = 150\n\n'
    printf '[metric.base]\ntype = "wired"\nnominal_cost = 96\nreceived = 2\nwindow = 3\n\n'
    printf '[route_selection]\nswitch_margin_percent = 5\nswitch_margin_metric = 8\nbetter_for_ms = 5000\n\n'
    if test -n "${origin}"; then printf '[[origins]]\ndestination = "%s"\nmetric = 0\n\n' "${origin}"; fi
    printf '[export]\nprotocol = 203\ndevice_only = false\nmanage_rules = false\n\n[[export.views]]\ntable = %s\n' "${table}"
  } >"${runtime}/${node}.toml"
}

write_config a 01:01:01:01:01:01:01:01 '["ab", "ac", "ae"]' 24101 2001:db8:a::1/128
write_config b 02:02:02:02:02:02:02:02 '["ba", "bd"]' 24102
write_config c 03:03:03:03:03:03:03:03 '["ca", "cd"]' 24103
write_config d 04:04:04:04:04:04:04:04 '["db", "dc", "df"]' 24104 2001:db8:d::1/128
write_config e 05:05:05:05:05:05:05:05 '["ea", "ef"]' 24105
write_config f 06:06:06:06:06:06:06:06 '["fe", "fd"]' 24106

table=24101
for node in "${nodes[@]}"; do
  ip -n "$(ns "${node}")" -6 rule add priority 1000 table "${table}"
  ip netns exec "$(ns "${node}")" "${daemon}" run --config "${runtime}/${node}.toml" \
    --control-socket "${runtime}/${node}.ctl" >"${runtime}/${node}.log" 2>&1 &
  pids+=("$!")
  table=$((table + 1))
done

wait_for_interface() {
  expected=$1 attempts=$2
  for ((attempt = 0; attempt < attempts; attempt++)); do
    if test -S "${runtime}/a.ctl" \
      && "${daemon}" routes --socket "${runtime}/a.ctl" --destination 2001:db8:d::1/128 \
        >"${runtime}/route.json" 2>/dev/null \
      && grep -q '"interface": "'"${expected}"'"' "${runtime}/route.json"; then
      return 0
    fi
    sleep 1
  done
  echo "route did not converge to ${expected}" >&2
  return 1
}

# RTT prefers the low-delay two-hop C path over the high-delay B path and the
# low-delay but longer E/F fallback.
wait_for_interface ac 45
ip netns exec "$(ns a)" ping -6 -n -c 2 -W 2 2001:db8:d::1 >/dev/null

# A degradation shorter than the configured dwell must not cause churn.
set_c_delay 35
sleep 3
set_c_delay 2
sleep 7
wait_for_interface ac 5

# A sustained latency reversal must eventually select the now-faster B path.
set_b_delay 2
set_c_delay 35
wait_for_interface ab 55
ip netns exec "$(ns a)" ping -6 -n -c 2 -W 2 2001:db8:d::1 >/dev/null

echo "babel-rs delayed multipath RTT selection E2E: PASS"
