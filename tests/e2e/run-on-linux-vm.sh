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

CARGO_HOME="${repo_root}/.local/cargo" RUSTUP_TOOLCHAIN=stable \
  "${cargo_bin}" build --release --package babel-rs

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
  "${ssh_host}:${remote_root}/"
case ${1:-all} in
  all)
    remote_tests="'${remote_root}/netns-babeld.sh' '${remote_root}/babel-rs' && '${remote_root}/netns-bird.sh' '${remote_root}/babel-rs' && '${remote_root}/netns-three-node.sh' '${remote_root}/babel-rs' && '${remote_root}/netns-rtt.sh' '${remote_root}/babel-rs' && '${remote_root}/netns-rtt-multipath.sh' '${remote_root}/babel-rs' && '${remote_root}/netns-lifecycle.sh' '${remote_root}/babel-rs' && '${remote_root}/netns-mtu-output.sh' '${remote_root}/babel-rs'"
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
  *) echo "usage: $0 [all|babeld|bird|three-node|rtt|rtt-multipath|lifecycle|mtu-output]" >&2; exit 2 ;;
esac
ssh "${ssh_args[@]}" "${ssh_host}" \
  "chmod 0700 '${remote_root}/babel-rs' '${remote_root}/netns-babeld.sh' '${remote_root}/netns-bird.sh' '${remote_root}/netns-three-node.sh' '${remote_root}/netns-rtt.sh' '${remote_root}/netns-rtt-multipath.sh' '${remote_root}/netns-lifecycle.sh' '${remote_root}/netns-mtu-output.sh' && ${remote_tests}"
