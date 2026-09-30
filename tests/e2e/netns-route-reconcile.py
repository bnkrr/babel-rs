#!/usr/bin/env python3
"""Run the exporter write/idempotence regression in a disposable namespace."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import uuid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument('--test-binary', type=Path)
    source.add_argument('--cargo-artifacts', type=Path)
    args = parser.parse_args()
    if os.geteuid() != 0:
        raise SystemExit('requires root on a disposable Linux test host')
    binary = args.test_binary
    if args.cargo_artifacts:
        artifacts = [json.loads(line) for line in args.cargo_artifacts.read_text().splitlines()]
        binaries = {item['executable'] for item in artifacts
                    if item.get('reason') == 'compiler-artifact'
                    and item.get('target', {}).get('name') == 'babel-rs'
                    and item.get('profile', {}).get('test') and item.get('executable')}
        if len(binaries) != 1:
            raise RuntimeError('Cargo must report exactly one Babel daemon test binary')
        binary = Path(binaries.pop())
    namespace = 'vb-routes-' + uuid.uuid4().hex[:8]
    subprocess.run(['ip', 'netns', 'add', namespace], check=True, timeout=5)
    inode = (Path('/run/netns') / namespace).stat().st_ino
    try:
        subprocess.run(['ip', 'netns', 'exec', namespace, str(binary.resolve()),
                        'linux::kernel_tests::kernel_route_reconciliation', '--exact', '--ignored', '--nocapture'],
                       check=True, timeout=30)
    finally:
        subprocess.run(['ip', 'netns', 'del', namespace], check=True, timeout=5)
        Path(f'/run/babel-rs/protocol-{inode}-203.lock').unlink(missing_ok=True)


if __name__ == '__main__':
    main()
