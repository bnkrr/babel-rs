#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cargo_bin=${BABEL_RS_CARGO_BIN:-cargo}
ssh_host=${BABEL_RS_E2E_HOST:?set BABEL_RS_E2E_HOST to an SSH-accessible Linux VM}
remote_root=${BABEL_RS_E2E_REMOTE_ROOT:-/tmp/babel-rs-e2e}

ssh_args=(-o ControlMaster=no -o ControlPath=none)
if [[ -n ${BABEL_RS_SSH_CONFIG:-} ]]; then
  ssh_args=(-F "${BABEL_RS_SSH_CONFIG}" "${ssh_args[@]}")
fi

steady_seconds=${BABEL_RS_STEADY_SECONDS:-3600}
if [[ ${1:-all} == steady-state ]] && { [[ ! $steady_seconds =~ ^[0-9]+$ ]] || (( 10#$steady_seconds < 120 || 10#$steady_seconds > 86400 )); }; then
  echo "BABEL_RS_STEADY_SECONDS must be in 120..86400" >&2
  exit 2
fi

CARGO_HOME="${repo_root}/.local/cargo" RUSTUP_TOOLCHAIN=stable \
  "${cargo_bin}" build --release --package babel-rs

test_assets=()
if [[ ${1:-all} == all || ${1:-all} == route-policy ]]; then
  CARGO_HOME="${repo_root}/.local/cargo" RUSTUP_TOOLCHAIN=stable \
    "${cargo_bin}" build --locked --release --package babel-router --example route_policy
  test_assets+=("${repo_root}/target/release/examples/route_policy")
fi
if [[ ${1:-all} == all || ${1:-all} == shutdown-recovery ]]; then
  fixture_dir="${repo_root}/.local/experiments/shutdown-recovery"
  mkdir -p "${fixture_dir}"
  "${CC:-cc}" -shared -fPIC -Wall -Wextra -Werror -O2 \
    "${repo_root}/tests/e2e/netlink-stall.c" -o "${fixture_dir}/netlink-stall.so" -ldl
  test_assets+=("${fixture_dir}/netlink-stall.so")
fi

ssh "${ssh_args[@]}" "${ssh_host}" "mkdir -p '${remote_root}'"
scp "${ssh_args[@]}" \
  "${repo_root}/target/release/babel-rs" \
  "${repo_root}/tests/e2e/netns-babeld.sh" \
  "${repo_root}/tests/e2e/netns-bird.sh" \
  "${repo_root}/tests/e2e/netns-lifecycle.sh" \
  "${repo_root}/tests/e2e/netns-mtu-output.sh" \
  "${repo_root}/tests/e2e/netns-rtt.sh" \
  "${repo_root}/tests/e2e/netns-rtt-multipath.sh" \
  "${repo_root}/tests/e2e/netns-three-node.sh" \
  "${repo_root}/tests/e2e/netns-capacity.py" \
  "${repo_root}/tests/e2e/netns-general.py" \
  "${repo_root}/tests/e2e/netns-rfc-boundaries.py" \
  "${repo_root}/tests/e2e/netns-route-policy.py" \
  "${repo_root}/tests/e2e/netns-mac-sadr.py" \
  "${repo_root}/tests/e2e/netns-state-restart.py" \
  "${repo_root}/tests/e2e/netns-shutdown-recovery.py" \
  "${repo_root}/tests/e2e/netns-control-clients.py" \
  "${repo_root}/tests/e2e/netns-steady-state.py" \
  "${repo_root}/tests/e2e/netns-combined-failures.py" \
  "${test_assets[@]}" \
  "${ssh_host}:${remote_root}/"
case ${1:-all} in
  all)
    remote_tests="'${remote_root}/netns-babeld.sh' '${remote_root}/babel-rs' && '${remote_root}/netns-bird.sh' '${remote_root}/babel-rs' && '${remote_root}/netns-three-node.sh' '${remote_root}/babel-rs' && '${remote_root}/netns-rtt.sh' '${remote_root}/babel-rs' && '${remote_root}/netns-rtt-multipath.sh' '${remote_root}/babel-rs' && '${remote_root}/netns-lifecycle.sh' '${remote_root}/babel-rs' && '${remote_root}/netns-mtu-output.sh' '${remote_root}/babel-rs' && python3 '${remote_root}/netns-capacity.py' '${remote_root}/babel-rs' && python3 '${remote_root}/netns-state-restart.py' '${remote_root}/babel-rs' && python3 '${remote_root}/netns-shutdown-recovery.py' '${remote_root}/babel-rs' '${remote_root}/netlink-stall.so'"
    remote_tests+=" && python3 '${remote_root}/netns-rfc-boundaries.py' '${remote_root}/babel-rs'"
    remote_tests+=" && python3 '${remote_root}/netns-route-policy.py' '${remote_root}/babel-rs' '${remote_root}/route_policy'"
    remote_tests+=" && python3 '${remote_root}/netns-general.py' '${remote_root}/babel-rs'"
    remote_tests+=" && python3 '${remote_root}/netns-mac-sadr.py' '${remote_root}/babel-rs'"
    remote_tests+=" && python3 '${remote_root}/netns-control-clients.py' '${remote_root}/babel-rs'"
    remote_tests+=" && python3 '${remote_root}/netns-combined-failures.py' '${remote_root}/babel-rs'"
    remote_tests+=" && python3 '${remote_root}/netns-steady-state.py' '${remote_root}/babel-rs' --seconds 120"
    ;;
  mac-sadr)
    remote_tests="python3 '${remote_root}/netns-mac-sadr.py' '${remote_root}/babel-rs'"
    ;;
  rfc-boundaries)
    remote_tests="python3 '${remote_root}/netns-rfc-boundaries.py' '${remote_root}/babel-rs'"
    ;;
  route-policy)
    remote_tests="python3 '${remote_root}/netns-route-policy.py' '${remote_root}/babel-rs' '${remote_root}/route_policy'"
    ;;
  general)
    remote_tests="python3 '${remote_root}/netns-general.py' '${remote_root}/babel-rs'"
    ;;
  combined-failures)
    remote_tests="python3 '${remote_root}/netns-combined-failures.py' '${remote_root}/babel-rs'"
    ;;
  steady-state)
    remote_tests="python3 '${remote_root}/netns-steady-state.py' '${remote_root}/babel-rs' --seconds '${steady_seconds}'"
    ;;
  control-clients)
    remote_tests="python3 '${remote_root}/netns-control-clients.py' '${remote_root}/babel-rs'"
    ;;
  shutdown-recovery)
    remote_tests="python3 '${remote_root}/netns-shutdown-recovery.py' '${remote_root}/babel-rs' '${remote_root}/netlink-stall.so'"
    ;;
  state-restart)
    remote_tests="python3 '${remote_root}/netns-state-restart.py' '${remote_root}/babel-rs'"
    ;;
  capacity)
    remote_tests="python3 '${remote_root}/netns-capacity.py' '${remote_root}/babel-rs'"
    ;;
  three-node)
    remote_tests="'${remote_root}/netns-three-node.sh' '${remote_root}/babel-rs'"
    ;;
  babeld)
    remote_tests="'${remote_root}/netns-babeld.sh' '${remote_root}/babel-rs'"
    ;;
  bird)
    remote_tests="'${remote_root}/netns-bird.sh' '${remote_root}/babel-rs'"
    ;;
  lifecycle)
    remote_tests="'${remote_root}/netns-lifecycle.sh' '${remote_root}/babel-rs'"
    ;;
  rtt)
    remote_tests="'${remote_root}/netns-rtt.sh' '${remote_root}/babel-rs'"
    ;;
  rtt-multipath)
    remote_tests="'${remote_root}/netns-rtt-multipath.sh' '${remote_root}/babel-rs'"
    ;;
  mtu-output)
    remote_tests="'${remote_root}/netns-mtu-output.sh' '${remote_root}/babel-rs'"
    ;;
  *) echo "usage: $0 [all|mac-sadr|route-policy|rfc-boundaries|general|babeld|bird|three-node|rtt|rtt-multipath|lifecycle|mtu-output|capacity|state-restart|shutdown-recovery|control-clients|combined-failures|steady-state]" >&2; exit 2 ;;
esac
ssh "${ssh_args[@]}" "${ssh_host}" \
  "chmod 0700 '${remote_root}/babel-rs' '${remote_root}/netns-babeld.sh' '${remote_root}/netns-bird.sh' '${remote_root}/netns-three-node.sh' '${remote_root}/netns-rtt.sh' '${remote_root}/netns-rtt-multipath.sh' '${remote_root}/netns-lifecycle.sh' '${remote_root}/netns-mtu-output.sh' && ${remote_tests}"
