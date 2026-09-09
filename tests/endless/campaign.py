"""Serial next-seed supervisor. Each child owns its diagnostics and cleanup."""
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import time


def next_action(code, outcome, startup_failures):
    """Never start another network until the previous cleanup is confirmed."""
    if not outcome.get('cleanup_ok', False):
        return 'cleanup-failed', startup_failures
    if code == 130:
        return 'stopped', startup_failures
    if code == 0:
        return 'complete', startup_failures
    if code != 1:
        return 'child-aborted', startup_failures
    startup_failures = 0 if outcome.get('initial_verified', False) else startup_failures + 1
    return ('startup-failures' if startup_failures >= 3 else 'next-seed'), startup_failures


def child_command(args, seed, artifacts):
    command = [sys.executable, str(Path(__file__).with_name('netns.py')), str(args.daemon.resolve())]
    for key in ('nodes', 'min_nodes', 'avg_degree', 'graph', 'rounds', 'changes',
                'settle_timeout', 'stable_seconds', 'probe_pairs', 'rss_growth_mib', 'mix', 'bird', 'babeld'):
        value = getattr(args, key)
        if value is not None:
            command.extend(['--' + key.replace('_', '-'), str(value)])
    return command + ['--seed', str(seed), '--on-failure', 'stop', '--artifacts', str(artifacts)]


def run_campaign(args, artifacts):
    artifacts.mkdir(parents=True, exist_ok=False)
    for name in ('campaign.py', 'netns.py', 'model.py', 'instances.py'):
        shutil.copyfile(Path(__file__).with_name(name), artifacts / name)
    child, stopping = None, False
    seed, attempts, failures, startup_failures = args.seed, 0, 0, 0

    def stop(signum, _frame):
        nonlocal stopping
        stopping = True
        if child is not None and child.poll() is None:
            child.send_signal(signum)  # Controller handles its own daemon cleanup.

    previous = {sig: signal.signal(sig, stop) for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)}

    def save(state, **extra):
        entry = {'state': state, 'attempts': attempts, 'failed_runs': failures,
                 'consecutive_startup_failures': startup_failures, 'seed': seed,
                 'observed_utc': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()), **extra}
        temporary = artifacts / 'campaign.json.tmp'
        temporary.write_text(json.dumps(entry, indent=2) + '\n')
        temporary.replace(artifacts / 'campaign.json')

    try:
        while not stopping:
            # Preserve all failure directories. Stop instead of filling the disk;
            # reserve room for a full bounded-log child and its failure snapshot.
            required = (256 + 4 * args.nodes) * 1024 * 1024
            if shutil.disk_usage(artifacts).free < required:
                save('insufficient-disk', required_free_bytes=required)
                return 1
            attempts += 1
            run_dir = artifacts / f'run-{attempts:06d}-seed-{seed}'
            command = child_command(args, seed, run_dir)
            save('starting', run_directory=str(run_dir))
            # Isolate terminal group signals; forward once to its controller.
            child = subprocess.Popen(command, start_new_session=True)
            if stopping:
                child.send_signal(signal.SIGTERM)
            save('running', run_directory=str(run_dir), child_pid=child.pid)
            code = child.wait()
            child = None
            outcome_path = run_dir / 'outcome.json'
            outcome = json.loads(outcome_path.read_text()) if outcome_path.exists() else {}
            action, startup_failures = next_action(code, outcome, startup_failures)
            if code not in (0, 130):
                failures += 1
            save(action, run_directory=str(run_dir), exit_code=code)
            if action != 'next-seed':
                return 130 if action == 'stopped' else (0 if action == 'complete' and failures == 0 else 1)
            if stopping:
                save('stopped', run_directory=str(run_dir), exit_code=code)
                return 130
            seed += 1
        save('stopped')
        return 130
    finally:
        if child is not None and child.poll() is None:
            child.terminate()
            try:
                child.wait(timeout=90)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait()
        for sig, handler in previous.items():
            signal.signal(sig, handler)
