#!/usr/bin/env python3
"""Root-only dual-stack FIB retention, restart deadlines and live adoption.

Runs the supplied locally built daemon in disposable network namespaces.
"""
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import time


def run(*args):
    result = subprocess.run(args, capture_output=True, text=True, timeout=10)
    if result.returncode:
        raise RuntimeError(f"{args}: {result.stderr}")
    return result.stdout


def main():
    assert os.geteuid() == 0
    daemon = str(Path(sys.argv[1]).resolve())
    namespaces = {n: f"vb-fib-{n}-{os.getpid()}" for n in "ab"}
    processes, created = [], []
    signal.signal(signal.SIGTERM, lambda *_: (_ for _ in ()).throw(KeyboardInterrupt()))
    targets = [('-4', '10.99.0.2/32', '10.0.0.2', '10.0.0.1/32', 1000096),
               ('-6', '2001:db8:99::2/128', 'fe80::b', '2001:db8:a::1/128', 1000000)]
    with tempfile.TemporaryDirectory(prefix='babel-fib.') as directory:
        root = Path(directory)

        def ip(n, *args):
            return run('ip', '-n', namespaces[n], *args)

        def config(n='a', policy='clear', window=12, origins=False, match='veth0', table=201):
            text = f'''router_id = "{('11' if n == 'a' else '22') * 8}"
state_file = "{root}/{n}.state"
shutdown_policy = "{policy}"
restart_recovery_seconds = {window}
[[interfaces]]
match = ["{match}"]
[interfaces.metric]
type = "wired"
nominal_cost = 96
'''
            if origins:
                for _, target, *_ in targets:
                    text += f'[[origins]]\ndestination = "{target}"\n'
            text += f'''[export]
protocol = 209
source_table_base = 1000000
[[export.views]]
table = {table}
'''
            (root / f'{n}.toml').write_text(text)

        def command(n='a', name='status'):
            with socket.socket(socket.AF_UNIX) as sock:
                sock.settimeout(2)
                sock.connect(str(root / f'{n}.ctl'))
                with sock.makefile('rwb') as stream:
                    json.loads(stream.readline())
                    stream.write((json.dumps(dict(api_version=1, id=1, command=name, params={}))+'\n').encode())
                    stream.flush()
                    reply = json.loads(stream.readline())
                    assert reply['ok'], reply
                    return reply['result']

        def wait(predicate, description, timeout=20):
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                try:
                    value = predicate()
                    if value:
                        return value
                except (FileNotFoundError, ConnectionRefusedError):
                    pass
                time.sleep(.1)
            raise AssertionError(f'timed out: {description}')

        def start(n='a', broken=False):
            ctl = root / f'{n}.ctl'
            if broken:
                ctl.unlink(missing_ok=True)
                ctl.mkdir()
            with (root / f'{len(processes)}-{n}.log').open('w') as log:
                proc = subprocess.Popen(['ip', 'netns', 'exec', namespaces[n], daemon,
                                         'run', '--config', str(root / f'{n}.toml'),
                                         '--control-socket', str(ctl)], stdout=log, stderr=log)
            processes.append(proc)
            if not broken:
                def ready():
                    assert proc.poll() is None, f'daemon exited {proc.returncode}'
                    return command(n)['export']['last_success_age_seconds'] is not None
                wait(ready, 'startup')
            return proc

        def stop(proc, sig=signal.SIGTERM):
            proc.send_signal(sig)
            result = proc.wait(timeout=8)
            assert result == (-signal.SIGKILL if sig == signal.SIGKILL else 0), result

        def routes(proto=209):
            return [r for family in ('-4', '-6')
                    for r in json.loads(ip('a', family, '-j', 'route', 'show', 'table', 'all', 'proto', str(proto)))]

        def rules():
            return [r for family in ('-4', '-6')
                    for r in json.loads(ip('a', family, '-j', '-details', 'rule', 'show'))
                    if str(r.get('protocol')) == '209']

        def seed(sources=False):
            for family, target, gateway, source, table in targets:
                for t in ([201, table] if sources else [201]):
                    ip('a', family, 'route', 'replace', target, 'via', gateway, 'dev', 'veth0',
                       'table', str(t), 'proto', '209', 'metric', '65631')
                if sources:
                    priority = 10000 + 128 - int(source.split('/')[1])
                    ip('a', family, 'rule', 'add', 'from', source, 'table', str(table),
                       'pref', str(priority), 'protocol', '209')

        def ping():
            for family, target, _, source, _ in targets:
                run('ip', 'netns', 'exec', namespaces['a'], 'ping', family, '-n', '-c', '1',
                    '-W', '1', '-I', source.split('/')[0], target.split('/')[0])

        def pending():
            return command()['export']['recovering_routes']

        try:
            for n, ns in namespaces.items():
                run('ip', 'netns', 'add', ns)
                created.append(ns)
                ip(n, 'link', 'set', 'lo', 'up')
            ip('a', 'link', 'add', 'veth0', 'type', 'veth', 'peer', 'name', 'peer0')
            ip('a', 'link', 'set', 'peer0', 'netns', namespaces['b'])
            ip('b', 'link', 'set', 'peer0', 'name', 'veth0')
            for n, host in [('a', 1), ('b', 2)]:
                ip(n, 'link', 'set', 'veth0', 'up')
                ip(n, 'addr', 'add', f'10.0.0.{host}/24', 'dev', 'veth0')
                ip(n, '-6', 'addr', 'add', f'2001:db8:a::{host}/64', 'dev', 'veth0', 'nodad')
                ip(n, '-6', 'addr', 'add', f'fe80::{n}/64', 'dev', 'veth0', 'nodad')
                for family in ('-4', '-6'):
                    ip(n, family, 'rule', 'add', 'pref', '20000', 'table', '201')
            for family, target, *_ in targets:
                ip('b', family, 'addr', 'add', target, 'dev', 'lo')
            ip('a', '-4', 'route', 'add', 'blackhole', '192.0.2.0/24', 'proto', '210')
            # Startup adopts only eligible routes; repeated crashes cannot reset the deadline.
            config()
            seed(sources=True)
            # Owned but out-of-scope entries must not be adopted.
            ip('a', 'link', 'add', 'excluded0', 'type', 'dummy')
            ip('a', 'link', 'set', 'excluded0', 'up')
            for family, prefix in [('-4', '198.51.100.1/32'), ('-6', '2001:db8:dead::1/128')]:
                ip('a', family, 'route', 'add', prefix, 'dev', 'veth0', 'table', '900', 'proto', '209', 'metric', '65631')
                ip('a', family, 'route', 'add', prefix, 'dev', 'excluded0', 'table', '201', 'proto', '209', 'metric', '65631')
                ip('a', family, 'route', 'add', 'unreachable', prefix, 'table', '201', 'proto', '209', 'metric', '131070')
            p = start()
            assert pending() == 4 and len(rules()) == 2 and len(routes()) == 4
            assert command()['selected_routes'] == 0, 'old FIB leaked into protocol RIB'
            ping()
            journal = root / 'a.state.fib-recovery.json'
            deadline = {r['deadline_ms'] for r in json.loads(journal.read_text())['pending']}
            for _ in range(2):
                time.sleep(1)
                stop(p, signal.SIGKILL)
                p = start()
                assert {r['deadline_ms'] for r in json.loads(journal.read_text())['pending']} == deadline
                assert len(routes()) == 4 and len(rules()) == 2
                ping()
            wait(lambda: not routes() and not rules(), 'original recovery deadline', 15)
            assert len(routes(210)) == 1
            stop(p)
            print('PASS dual-stack forwarding, source rules, crash-loop deadline, foreign ownership', flush=True)

            # An orderly retain exits without removing the old FIB; clear removes it.
            config(policy='retain', window=60)
            seed(sources=True)
            p = start()
            stop(p)
            assert len(routes()) == 4 and len(rules()) == 2
            ping()
            config(window=60)
            p = start()
            assert pending() == 4
            stop(p)
            assert not routes() and not rules()
            print('PASS retain exit, startup adoption and default clear', flush=True)

            # A critical control-service failure must preserve forwarding too.
            seed()
            p = start(broken=True)
            assert p.wait(timeout=8) != 0
            assert len(routes()) == 2
            ping()
            (root / 'a.ctl').rmdir()
            p = start()
            stop(p)
            assert not routes()
            print('PASS critical task failure retains FIB', flush=True)

            # Relearning transfers FIB ownership to the live RIB before the deadline.
            seed()
            p = start()
            assert pending() == 2
            config('b', origins=True, window=60, policy='retain')
            peer = start('b')
            wait(lambda: pending() == 0 and len(routes()) == 2, 'live route confirmation', 45)
            ping()
            assert json.loads(journal.read_text())['pending'] == []
            stop(peer)
            time.sleep(1)
            assert len([r for r in routes() if r.get('type', 'unicast') == 'unicast']) == 2
            ping()
            config('b', origins=True, window=60)
            peer = start('b')
            time.sleep(2)
            stop(peer)
            wait(lambda: not [r for r in routes() if r.get('type', 'unicast') == 'unicast'],
                 'withdrawal after adoption', 20)
            stop(p)
            print('PASS fresh Babel routes confirm retained FIB; later withdrawal works', flush=True)

            config(window=0)
            seed()
            p = start()
            assert not routes()
            stop(p)
            print('PASS recovery can be disabled', flush=True)

            for change in ({'match': 'absent*'}, {'table': 202}):
                config(window=60)
                seed()
                p = start()
                assert pending() == 2
                config(window=60, **change)
                command(name='reload')
                wait(lambda: not routes(), 'explicit policy change revokes old FIB')
                stop(p)
            print('PASS interface removal and export table changes revoke retention', flush=True)

            config(window=60)
            seed()
            journal.write_text('{invalid')
            with (root / 'corrupt.log').open('w') as log:
                p = subprocess.Popen(['ip', 'netns', 'exec', namespaces['a'], daemon,
                                      'run', '--config', str(root / 'a.toml')], stdout=log, stderr=log)
            processes.append(p)
            assert p.wait(timeout=8) != 0
            assert len(routes()) == 2, 'startup error must not erase forwarding state'
            print('PASS corrupt recovery journal refuses startup without resetting deadline', flush=True)
        except BaseException:
            for log in root.glob('*.log'):
                print(log.name, log.read_text()[-3000:], file=sys.stderr)
            raise
        finally:
            for proc in processes:
                if proc.poll() is None:
                    proc.kill()
                    proc.wait(timeout=5)
            for ns in reversed(created):
                run('ip', 'netns', 'del', ns)


if __name__ == '__main__':
    main()
