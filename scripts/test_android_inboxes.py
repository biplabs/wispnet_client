#!/usr/bin/env python3
"""Opt-in real named-inbox SAF/QUIC acceptance on a fresh disposable emulator."""
import argparse
import base64
import hashlib
import json
from pathlib import Path
import secrets
import shlex
import subprocess
import sys
import tempfile
import threading

from android_test_relay import android_test_relay, write_private_json

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'apps/node/desktop'))
from wispnet_desktop.api import NodeAPI


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--serial', required=True)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--apk', type=Path, default=ROOT / 'apps/flutter/build/app/outputs/flutter-apk/app-debug.apk')
    parser.add_argument('--test-apk', type=Path, default=ROOT / 'apps/flutter/build/app/outputs/apk/androidTest/debug/app-debug-androidTest.apk')
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    app = 'app.wispnet.android'

    def adb(*command, **kwargs):
        return subprocess.run(['adb', '-s', args.serial, *map(str, command)], check=True, capture_output=True, **kwargs)

    if adb('shell', 'getprop', 'ro.kernel.qemu', text=True).stdout.strip() != '1':
        raise SystemExit('This fixture only runs on a disposable emulator.')
    adb('shell', 'wm', 'size', '480x960')
    adb('shell', 'wm', 'density', '180')
    for setting in ('window_animation_scale', 'transition_animation_scale', 'animator_duration_scale'):
        adb('shell', 'settings', 'put', 'global', setting, '0')
    adb('shell', 'am', 'force-stop', app)
    adb('shell', 'am', 'force-stop', 'com.android.documentsui')
    for apk in (args.apk, args.test_apk):
        if not apk.is_file():
            raise SystemExit(f'Build current x86_64 fixtures first: {apk}')
        adb('install', '-r', apk)
    adb('shell', 'pm', 'clear', app)
    adb('shell', 'pm', 'grant', app, 'android.permission.POST_NOTIFICATIONS')
    prefix = 'WispnetInboxTest-' + secrets.token_hex(6)
    folders = [prefix + '-a', prefix + '-b']
    for folder in folders:
        adb('shell', 'mkdir', '-p', '/sdcard/Documents/' + folder)
    binary = ROOT / 'target/debug/wispnet'
    stopped = threading.Event()
    errors = []
    with tempfile.TemporaryDirectory(prefix='wispnet-android-inboxes-') as directory, android_test_relay(args.serial) as (relay_url, relay_ca):
        temporary = Path(directory)
        sink = temporary / 'desktop-inbox'
        sink.mkdir(mode=0o700)
        config = temporary / 'host.toml'
        config.write_text(f'schema_version=2\ndevice_name="named-inbox-fixture"\nstate_dir="{temporary / "state"}"\n'
                          'log_level="info"\n[transport]\nbind_addr="127.0.0.1:0"\n'
                          f'relay_url={json.dumps(relay_url)}\nrelay_ca_file={json.dumps(str(relay_ca))}\n')
        subprocess.run([binary, 'host-init', '--config', config], check=True, capture_output=True)
        with (args.output / 'host.log').open('w') as log:
            process = subprocess.Popen([binary, 'agent', '--config', config], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
            drainer = None
            try:
                line = process.stderr.readline()
                started = json.loads(line)['fields']
                if started['event'] != 'started':
                    raise RuntimeError(f'Fixture startup failed: {line}')
                port = int(started['bind_addr'].rsplit(':', 1)[1])
                log.write(line)

                def drain():
                    for line in process.stderr:
                        log.write(line)
                drainer = threading.Thread(target=drain, daemon=True)
                drainer.start()
                api = NodeAPI(config)
                host = api.request(op='status')['host']['host']

                def mutate(**operation):
                    operation['expected_revision'] = api.request(op='status')['host']['revision']
                    return api.request(op='mutate', operation=operation)

                mutate(op='file_inbox', id='desktop-inbox', root=str(sink), allow=[])
                invitation_file = temporary / 'invitation.json'
                subprocess.run([binary, 'host-invite', '--config', config, '--output', invitation_file], check=True, capture_output=True)
                invitation = json.loads(invitation_file.read_text())
                invitation['direct_hint'] = f'10.0.2.2:{port}'
                encoded = 'wispnet1:' + base64.urlsafe_b64encode(json.dumps(invitation).encode()).decode().rstrip('=')

                def cli(*command, success=True):
                    result = subprocess.run([binary, *map(str, command), '--config', config], capture_output=True, text=True, timeout=100)
                    if (result.returncode == 0) != success:
                        raise AssertionError(f'Unexpected CLI result {command}: {result.stdout}\n{result.stderr}')
                    return result

                def approve():
                    try:
                        while not stopped.wait(.1):
                            for pending in api.request(op='status')['invitations']:
                                if pending['state'] == 'pending':
                                    mutate(op='approve_invitation', invitation=pending['invitation'])
                                    mutate(op='grant_service', service='desktop-inbox', peer=pending['peer'])
                    except Exception as failure:
                        errors.append(f'pairing: {failure}')

                def sends():
                    handled = set()
                    while not stopped.wait(.2):
                        try:
                            ready = json.loads(adb('exec-out', 'run-as', app, 'cat', 'cache/android-inboxes-ready.json').stdout)
                        except (subprocess.CalledProcessError, ValueError):
                            continue
                        phase, peer = ready['phase'], ready['peer']
                        if phase in handled:
                            continue
                        handled.add(phase)
                        result = {'success': False}
                        try:
                            catalog = api.request(op='catalog', host=peer)
                            selected = [(entry['id'], entry['kind']) for entry in catalog['services']]
                            expected = {'first': [('inbox-a', 'file_inbox')],
                                        'both': [('inbox-a', 'file_inbox'), ('inbox-b', 'file_inbox')],
                                        'revoked': [('inbox-b', 'file_inbox')]}[phase]
                            if selected != expected or 'content://' in json.dumps(catalog) or '/data/' in json.dumps(catalog):
                                raise AssertionError(f'Incorrect scoped/private catalog: {catalog}')
                            data = {'first': b'first inbox\0' * 90000, 'both': b'second inbox\xff' * 90000,
                                    'revoked': b'other inbox still works'}[phase]
                            source = temporary / ('source-' + phase)
                            source.write_bytes(data)
                            inbox = 'inbox-a' if phase == 'first' else 'inbox-b'
                            destination = 'still.bin' if phase == 'revoked' else 'same.bin'
                            uploaded = json.loads(cli('files', 'put', peer, inbox, source, destination).stdout)
                            if uploaded['file']['sha256'] != hashlib.sha256(data).hexdigest() or uploaded['file']['size_bytes'] != len(data):
                                raise AssertionError('Named-inbox native receipt changed bytes')
                            if phase == 'first':
                                cli('files', 'put', peer, 'inbox-b', source, 'same.bin', success=False)
                                cli('files', 'ls', peer, 'inbox-a', success=False)
                            elif phase == 'revoked':
                                denied = json.loads(cli('files', 'put', peer, 'inbox-a', source, 'revoked.bin', success=False).stdout)
                                if denied['error']['code'] != 'PERMISSION_DENIED':
                                    raise AssertionError('Revoked inbox failed to deny uploads')
                            result = {'success': True, 'file': uploaded['file']}
                        except Exception as failure:
                            errors.append(f'{phase}: {failure}')
                            result['error'] = str(failure)
                        write_private_json(adb, app, f'cache/android-inboxes-host-{phase}.json', result)

                threading.Thread(target=approve, daemon=True).start()
                threading.Thread(target=sends, daemon=True).start()
                command = ['am', 'instrument', '-w', '-r', '-e', 'namedInboxes', 'true',
                           '-e', 'receiving_a', folders[0], '-e', 'receiving_b', folders[1],
                           '-e', 'host', host, '-e', 'invitation', encoded,
                           'app.wispnet.android.test/com.biplabs.wispnet.FilesTransferInstrumentation']
                run = adb('shell', ' '.join(map(shlex.quote, command)), text=True, timeout=600)
                (args.output / 'instrumentation.log').write_text(run.stdout + run.stderr)
                report = json.loads(adb('exec-out', 'run-as', app, 'cat', 'cache/android-inboxes-result.json').stdout)
                if errors or 'INSTRUMENTATION_CODE: -1' not in run.stdout or not report.get('success'):
                    raise RuntimeError(f'Android named inbox checks failed: {errors} {report}')
                outbound = (sink / 'android-to-named.bin').read_bytes()
                if len(outbound) != report['outbound']['size_bytes'] or hashlib.sha256(outbound).hexdigest() != report['outbound']['sha256']:
                    raise AssertionError('Android-to-desktop named-inbox bytes changed')
                (args.output / 'verification.json').write_text(json.dumps(report, indent=2) + '\n')
                print(json.dumps(report, indent=2))
            finally:
                stopped.set()
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
                if drainer is not None:
                    drainer.join(timeout=5)
                for folder in folders:
                    adb('shell', 'rm', '-rf', '/sdcard/Documents/' + folder)


if __name__ == '__main__':
    main()
