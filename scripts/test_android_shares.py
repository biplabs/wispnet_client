#!/usr/bin/env python3
"""Opt-in Android SAF source-share acceptance on a disposable emulator.

Uses a real system tree grant, isolated desktop identity and native QUIC. No
installed desktop policy or physical Android device is modified.
"""
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
        return subprocess.run(['adb', '-s', args.serial, *map(str, command)],
                              check=True, capture_output=True, **kwargs)

    if adb('shell', 'getprop', 'ro.kernel.qemu', text=True).stdout.strip() != '1':
        raise SystemExit('This fixture is restricted to disposable emulators.')
    # Keep software-rendered disposable emulators responsive during local builds.
    adb('shell', 'wm', 'size', '480x960')
    adb('shell', 'wm', 'density', '180')
    for setting in ('window_animation_scale', 'transition_animation_scale', 'animator_duration_scale'):
        adb('shell', 'settings', 'put', 'global', setting, '0')
    # A prior failed tree picker can otherwise remain above startActivitySync.
    adb('shell', 'am', 'force-stop', app)
    adb('shell', 'am', 'force-stop', 'com.android.documentsui')
    for apk in (args.apk, args.test_apk):
        if not apk.is_file():
            raise SystemExit(f'Build current Android artifacts first: {apk}')
        adb('install', '-r', apk)
    # Only this disposable emulator fixture receives a fresh temporary identity.
    adb('shell', 'pm', 'clear', app)
    adb('shell', 'pm', 'grant', app, 'android.permission.POST_NOTIFICATIONS')
    phases = ['download', 'disabled', 'enabled', 'revoked', 'expired']
    adb('shell', 'run-as', app, 'rm', '-f', 'cache/android-shares-ready.json',
        'cache/android-shares-result.json', *[f'cache/android-shares-host-{phase}.json' for phase in phases])
    folder_name = 'WispnetShareTest-' + secrets.token_hex(6)
    device_folder = '/sdcard/Documents/' + folder_name
    adb('shell', 'mkdir', '-p', device_folder + '/nested')
    binary = ROOT / 'target/debug/wispnet'
    stopped = threading.Event()
    errors = []
    process = None
    with tempfile.TemporaryDirectory(prefix='wispnet-android-shares-') as directory:
        temporary = Path(directory)
        files = {'model.bin': bytes(range(256)) * 20480 + b'SAF checkpoint',
                 'nested/資料😀.bin': 'Unicode checkpoint 資料 😀'.encode(),
                 'empty.bin': b''}
        # Exceed the source adapter's bounded version cache so folder manifests
        # exercise lazy rebuilding of an evicted, still-pinned file version.
        for index in range(140):
            files[f'page-{index:02}.txt'] = f'entry {index}\n'.encode()
        for path, content in files.items():
            local = temporary / ('fixture-' + hashlib.sha256(path.encode()).hexdigest())
            local.write_bytes(content)
            adb('push', local, device_folder + '/' + path)
        expected = {path: {'size_bytes': len(content), 'sha256': hashlib.sha256(content).hexdigest()}
                    for path, content in files.items()}
        config = temporary / 'host.toml'
        config.write_text(f'schema_version=2\ndevice_name="android-share-reader"\nstate_dir="{temporary / "state"}"\n'
                          'log_level="info"\n[transport]\nbind_addr="127.0.0.1:0"\nrelay_url="https://relay.wispnet.app"\n')
        subprocess.run([binary, 'host-init', '--config', config], check=True, capture_output=True)
        with (args.output / 'host.log').open('w') as log:
            process = subprocess.Popen([binary, 'agent', '--config', config], stdout=subprocess.DEVNULL,
                                       stderr=subprocess.PIPE, text=True)
            try:
                line = process.stderr.readline()
                started = json.loads(line)['fields']
                if started['event'] != 'started':
                    raise RuntimeError(f'Isolated fixture did not start: {line}')
                port = int(started['bind_addr'].rsplit(':', 1)[1])
                log.write(line)
                threading.Thread(target=lambda: [log.write(line) for line in process.stderr], daemon=True).start()
                api = NodeAPI(config)
                host = api.request(op='status')['host']['host']
                invitation_file = temporary / 'invitation.json'
                subprocess.run([binary, 'host-invite', '--config', config, '--output', invitation_file], check=True, capture_output=True)
                invitation = json.loads(invitation_file.read_text())
                invitation['direct_hint'] = f'10.0.2.2:{port}'
                encoded = 'wispnet1:' + base64.urlsafe_b64encode(json.dumps(invitation).encode()).decode().rstrip('=')

                def cli(*command, success=True):
                    completed = subprocess.run([binary, *map(str, command), '--config', config],
                                               capture_output=True, text=True, timeout=100)
                    if (completed.returncode == 0) != success:
                        raise AssertionError(f'Unexpected CLI result {command}: {completed.stdout}\n{completed.stderr}')
                    return completed

                def approve():
                    try:
                        while not stopped.wait(.1):
                            status = api.request(op='status')
                            for pending in status['invitations']:
                                if pending['state'] == 'pending':
                                    api.request(op='mutate', operation=dict(op='approve_invitation',
                                        expected_revision=status['host']['revision'], invitation=pending['invitation']))
                    except Exception as failure:
                        errors.append(f'pairing: {failure}')

                def pull():
                    handled = set()
                    while not stopped.wait(.2):
                        try:
                            ready = json.loads(adb('exec-out', 'run-as', app, 'cat', 'cache/android-shares-ready.json').stdout)
                        except (subprocess.CalledProcessError, ValueError):
                            continue
                        phase, peer = ready['phase'], ready['peer']
                        if phase in handled:
                            continue
                        handled.add(phase)
                        result = {'success': False}
                        try:
                            catalog = api.request(op='catalog', host=peer)
                            if phase in ('download', 'enabled', 'expired'):
                                if [s['id'] for s in catalog['services']] != ['checkpoints']:
                                    raise AssertionError(f'Per-share catalog exposed unintended services: {catalog}')
                            else:
                                if catalog['services']:
                                    raise AssertionError(f'Disabled/revoked share remained in catalog: {catalog}')
                            if phase == 'download':
                                root = cli('files', 'ls', peer, 'checkpoints')
                                pages = [json.loads(line) for line in root.stdout.splitlines()]
                                entries = [entry for page in pages for entry in page['entries']]
                                if len(pages) < 2 or len(entries) != len({path.split('/')[0] for path in files}):
                                    raise AssertionError('Bounded folder pagination lost source entries')
                                nested = json.loads(cli('files', 'ls', peer, 'checkpoints', 'nested').stdout)
                                if nested['entries'][0]['path'] != 'nested/資料😀.bin':
                                    raise AssertionError('Unicode nested source path changed')
                                verified = {}
                                for path in ('model.bin', 'nested/資料😀.bin', 'empty.bin'):
                                    output = temporary / ('download-' + hashlib.sha256(path.encode()).hexdigest())
                                    receipt = json.loads(cli('files', 'get', peer, 'checkpoints', path, output).stdout)
                                    if receipt['file']['sha256'] != expected[path]['sha256'] or output.read_bytes() != files[path]:
                                        raise AssertionError(f'Native SAF download checksum mismatch: {path}')
                                    verified[path] = expected[path]
                                cli('files', 'ls', peer, 'private', success=False)
                                denial = json.loads(cli('files', 'put', peer, '', output, 'unsolicited.bin', success=False).stdout)
                                if denial['error']['code'] != 'PERMISSION_DENIED':
                                    raise AssertionError('Source sharing changed inbox authorization')
                                result = {'success': True, 'files': verified}
                            elif phase == 'enabled':
                                output = temporary / 'reenabled.bin'
                                cli('files', 'get', peer, 'checkpoints', 'model.bin', output)
                                if output.read_bytes() != files['model.bin']:
                                    raise AssertionError('Reenabled source download changed bytes')
                                result = {'success': True}
                            else:
                                output = temporary / f'{phase}.bin'
                                failure = json.loads(cli('files', 'get', peer, 'checkpoints', 'model.bin', output, success=False).stdout)
                                if output.exists() or failure['error']['code'] != 'PERMISSION_DENIED':
                                    raise AssertionError(f'{phase} source did not fail closed: {failure}')
                                result = {'success': True}
                        except Exception as failure:
                            errors.append(f'{phase}: {failure}')
                            result['error'] = str(failure)
                        adb('shell', 'run-as', app, 'sh', '-c',
                            shlex.quote(f'cat > cache/android-shares-host-{phase}.json'), input=json.dumps(result).encode())

                threading.Thread(target=approve, daemon=True).start()
                threading.Thread(target=pull, daemon=True).start()
                command = ['am', 'instrument', '-w', '-r', '-e', 'sourceShares', 'true',
                           '-e', 'shared_folder', folder_name, '-e', 'host', host, '-e', 'invitation', encoded,
                           'app.wispnet.android.test/com.biplabs.wispnet.FilesTransferInstrumentation']
                run = adb('shell', ' '.join(map(shlex.quote, command)), text=True, timeout=600)
                (args.output / 'instrumentation.log').write_text(run.stdout + run.stderr)
                raw = adb('exec-out', 'run-as', app, 'cat', 'cache/android-shares-result.json').stdout
                report = json.loads(raw)
                if errors or 'INSTRUMENTATION_CODE: -1' not in run.stdout or not report.get('success'):
                    raise RuntimeError(f'Android source share checks failed: {errors} {report}')
                (args.output / 'verification.json').write_text(json.dumps(report, indent=2, ensure_ascii=False) + '\n')
                print(json.dumps(report, indent=2, ensure_ascii=False))
            finally:
                stopped.set()
                if process is not None:
                    process.terminate()
                    try:
                        process.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()
                adb('shell', 'rm', '-rf', device_folder)


if __name__ == '__main__':
    main()
