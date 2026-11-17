"""Disposable TLS relay and private debug-app trust for emulator integration."""

from contextlib import contextmanager
import json
from pathlib import Path
import select
import shlex
import subprocess
import tempfile
from urllib.parse import urlsplit

ROOT = Path(__file__).resolve().parents[1]
APP = 'app.wispnet.android'


def write_private_json(adb, app, path, value):
    temporary = shlex.quote(path + '.tmp')
    command = f'cat > {temporary} && mv {temporary} {shlex.quote(path)}'
    adb('shell', 'run-as', app, 'sh', '-c', shlex.quote(command), input=json.dumps(value).encode())


@contextmanager
def android_test_relay(serial):
    def adb(*args, **kwargs):
        return subprocess.run(['adb', '-s', serial, *args], check=True,
                              capture_output=True, timeout=20, **kwargs)
    if adb('shell', 'getprop', 'ro.kernel.qemu').stdout.strip() != b'1':
        raise RuntimeError('The local relay fixture requires a disposable emulator')
    with tempfile.TemporaryDirectory(prefix='wispnet-emulator-relay-') as directory:
        ca = Path(directory) / 'relay.pem'
        process = subprocess.Popen([ROOT / 'target/debug/examples/relay_fixture', '--ca-file', ca],
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        port = None
        try:
            if not select.select([process.stdout], [], [], 15)[0]:
                raise RuntimeError('Local relay startup timed out')
            event = json.loads(process.stdout.readline())
            if event['event'] != 'started':
                raise RuntimeError('Local relay did not start')
            url = event['relay_url']
            candidate = urlsplit(url).port
            adb('shell', 'am', 'force-stop', APP)
            adb('reverse', '--no-rebind', f'tcp:{candidate}', f'tcp:{candidate}')
            port = candidate
            adb('shell', 'run-as', APP, 'mkdir', '-p', 'files')
            # Only public trust material is copied, never the relay's private key.
            adb('shell', 'run-as', APP, 'sh', '-c', '"cat > files/test-relay.pem"', input=ca.read_bytes())
            adb('shell', 'run-as', APP, 'sh', '-c', '"cat > files/test-relay.json"',
                input=json.dumps({'relay': url}).encode())
            adb('shell', 'run-as', APP, 'chmod', '600', 'files/test-relay.pem', 'files/test-relay.json')
            yield url, ca
        finally:
            try:
                adb('shell', 'am', 'force-stop', APP)
                adb('shell', 'run-as', APP, 'rm', '-f', 'files/test-relay.json', 'files/test-relay.pem')
            finally:
                try:
                    if port is not None:
                        adb('reverse', '--remove', f'tcp:{port}')
                finally:
                    process.terminate()
                    try:
                        process.communicate(timeout=10)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.communicate()
