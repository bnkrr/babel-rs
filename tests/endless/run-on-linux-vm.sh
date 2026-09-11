#!/usr/bin/env bash
set -euo pipefail
repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "${repo_root}"
for argument in "$@"; do
  case "$argument" in
    --help|-h|--plan)
      exec env PYTHONDONTWRITEBYTECODE=1 python3 "${repo_root}/tests/endless/netns.py" "$@"
      ;;
  esac
done
PYTHONDONTWRITEBYTECODE=1 python3 "${repo_root}/tests/endless/netns.py" --validate "$@"
ssh_host=${BABEL_RS_E2E_HOST:?set BABEL_RS_E2E_HOST to a disposable root Linux VM}
remote_root=${BABEL_RS_ENDLESS_REMOTE_ROOT:-/tmp/babel-rs-endless}
ssh_args=(-o ControlMaster=no -o ControlPath=none)
if [[ -n ${BABEL_RS_SSH_CONFIG:-} ]]; then
  ssh_args=(-F "${BABEL_RS_SSH_CONFIG}" "${ssh_args[@]}")
fi
cargo_bin=${BABEL_RS_CARGO_BIN:-cargo}
"${cargo_bin}" build --locked --release -p babel-rs
target_dir=$("${cargo_bin}" metadata --no-deps --format-version 1 |
  python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
# A separate asset directory avoids overwriting a binary used by regular E2E.
mkdir_command=$(python3 -c 'import shlex,sys; print(shlex.join(["mkdir", "-p", sys.argv[1]]))' "${remote_root}")
ssh "${ssh_args[@]}" "${ssh_host}" "${mkdir_command}"
scp "${ssh_args[@]}" "${target_dir}/release/babel-rs" \
  "${repo_root}/tests/endless/netns.py" "${repo_root}/tests/endless/model.py" \
  "${repo_root}/tests/endless/campaign.py" \
  "${repo_root}/tests/endless/instances.py" \
  "${ssh_host}:${remote_root}/"
remote_command=$(python3 - "${remote_root}" "$@" <<'PY'
import shlex
import sys
root = sys.argv[1]
print("cd " + shlex.quote(root) + " && exec " + shlex.join([
    "env", "PYTHONDONTWRITEBYTECODE=1", "python3", root + "/netns.py", root + "/babel-rs", *sys.argv[2:]]))
PY
)
exec ssh -tt "${ssh_args[@]}" "${ssh_host}" "${remote_command}"
