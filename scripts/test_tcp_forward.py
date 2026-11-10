"""Real running-node TCP forwarding and HTTP WebSocket upgrade regression."""
import base64
import contextlib
import fcntl
import hashlib
import json
from pathlib import Path
import socket
import socketserver
import subprocess
import tempfile
import threading
import time
import unittest
from urllib.parse import urlsplit
from test_node_desktop import NativeNodeAPI, BINARY


class ForwardingTests(unittest.TestCase):
    def test_tcp_lifecycle_and_websocket_through_running_nodes(self):
        processes = []

        class Echo(socketserver.BaseRequestHandler):
            def handle(self):
                data = self.request.recv(65536)
                if data.startswith(b'GET '):
                    while b'\r\n\r\n' not in data:
                        data += self.request.recv(65536)
                    key = next(l.split(b':', 1)[1].strip() for l in data.split(b'\r\n') if l.lower().startswith(b'sec-websocket-key:'))
                    accept = base64.b64encode(hashlib.sha1(key + b'258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest())
                    self.request.sendall(b'HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ' + accept + b'\r\n\r\n')
                    frame = self.request.recv(9)
                    while len(frame) < 9:
                        part = self.request.recv(9-len(frame))
                        if not part:
                            return
                        frame += part
                    payload = bytes(frame[6+i] ^ frame[2+i % 4] for i in range(3))
                    self.request.sendall(b'\x82\x03' + payload)
                    return
                while data:
                    self.request.sendall(data)
                    data = self.request.recv(65536)

        class Server(socketserver.ThreadingTCPServer):
            daemon_threads = True

        with tempfile.TemporaryDirectory() as tmp, Server(('127.0.0.1', 0), Echo) as echo:
            threading.Thread(target=echo.serve_forever, daemon=True).start()
            root = Path(tmp)

            def cli(config, *args):
                return subprocess.check_output([str(BINARY), args[0], '--config', str(config), *map(str, args[1:])], text=True, timeout=20)

            def node(name):
                with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
                    s.bind(('127.0.0.1', 0))
                    port = s.getsockname()[1]
                state = root / name
                config = root / (name + '.toml')
                config.write_text(f'schema_version=2\ndevice_name="{name}"\nstate_dir="{state}"\nlog_level="error"\n[transport]\nbind_addr="127.0.0.1:{port}"\nrelay_url="https://relay.example"\nmax_streams=256\nmax_streams_per_connection=128\n')
                peer = cli(config, 'host-init').split('Endpoint ID: ')[1].strip()
                proc = subprocess.Popen([str(BINARY), 'agent', '--config', str(config)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                processes.append(proc)
                deadline = time.monotonic() + 10
                while not (state / 'node.sock').exists():
                    self.assertIsNone(proc.poll())
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(.05)
                return config, peer, port, NativeNodeAPI(config)

            try:
                ca, pa, _, a = node('client')
                cb, pb, pbport, b = node('server')
                invitation = root / 'invite.json'
                cli(cb, 'host-invite', '--output', invitation)
                invite = json.loads(invitation.read_text())
                invite['direct_hint'] = f'127.0.0.1:{pbport}'
                a.request(op='pair', host=pb, invitation=json.dumps(invite))
                b.request(op='mutate', operation={'op':'approve_invitation','expected_revision':0,'invitation':invite['invitation']})

                def mutate(operation):
                    revision = b.request(op='status')['host']['revision']
                    b.request(op='mutate', operation={**operation, 'expected_revision':revision})

                for name, kind in [('database', 'tcp'), ('socket', 'http')]:
                    mutate({'op':'service','id':name,'kind':kind,'backend':f'127.0.0.1:{echo.server_address[1]}','allow':[pa]})

                def catalog():
                    for attempt in range(41):
                        try:
                            return a.request(op='catalog', host=pb)
                        except ValueError as error:
                            if 'Catalog refresh busy; retry this host' not in str(error) or attempt == 40:
                                raise
                            time.sleep(.025)

                def opening(name):
                    service = next(s for s in catalog()['services'] if s['id']==name)
                    return a.request(op='open',host=pb,service=name,incarnation=service['incarnation'])['url']

                with socket.socket() as free:
                    free.bind(('127.0.0.1', 0))
                    gateway_port = free.getsockname()[1]
                a.request(op='configure_access', settings={'registry_port':gateway_port})
                a.request(op='enable')
                url = opening('database')
                self.assertEqual(url, opening('database'))
                resolved = a.request(op='resolve_link', link=f'wispnet://{pb}/database')
                self.assertEqual(resolved['url'], url)
                self.assertEqual(resolved['kind'], 'tcp')
                # A local snapshot writer must not tear down authorized forwards.
                with (root / 'client' / 'catalog-baselines' / f'{pb}.lock').open('r+b') as lock:
                    fcntl.flock(lock.fileno(), fcntl.LOCK_EX)
                    def release_baseline():
                        time.sleep(.15)
                        fcntl.flock(lock.fileno(), fcntl.LOCK_UN)
                    unlock = threading.Thread(target=release_baseline)
                    unlock.start()
                    self.assertEqual(a.request(op='resolve_link', link=f'wispnet://{pb}/database')['url'], url)
                    unlock.join()

                # Persistent database sockets must exceed the former 32-slot cap
                # without starving catalog/control operations on the same peer.
                parsed = urlsplit(url)
                with contextlib.ExitStack() as held:
                    sockets = []
                    for i in range(40):
                        stream = held.enter_context(socket.create_connection((parsed.hostname, parsed.port), timeout=8))
                        payload = f'held-{i}'.encode()
                        stream.sendall(payload)
                        self.assertEqual(stream.recv(len(payload)), payload)
                        sockets.append(stream)
                    self.assertTrue(catalog()['services'])
                    for stream in sockets:
                        stream.sendall(b'again')
                        self.assertEqual(stream.recv(5), b'again')

                service = next(s for s in catalog()['services'] if s['id'] == 'database')
                with self.assertRaises(ValueError):
                    a.request(op='open', host=pb, service='database', incarnation='0' * 64)
                parsed = urlsplit(url)
                self.assertEqual(parsed.hostname, '127.0.0.1')
                links = json.loads(cli(ca, 'service-links', '--host', pb, '--service', 'database'))
                self.assertEqual(links['url'], url)
                with socket.create_connection((parsed.hostname, parsed.port), timeout=8) as one, socket.create_connection((parsed.hostname, parsed.port), timeout=8) as two:
                    for connection in [one, two]:
                        payload = bytes(range(256)) * 256
                        connection.sendall(payload)
                        received = b''
                        while len(received) < len(payload):
                            part = connection.recv(65536)
                            self.assertTrue(part)
                            received += part
                        self.assertEqual(received, payload)
                    mutate({'op':'revoke_service','service':'database','peer':pa})
                    self.assertEqual(one.recv(1), b'')
                    self.assertEqual(two.recv(1), b'')
                with self.assertRaises((OSError, ValueError, StopIteration)):
                    opening('database')
                with self.assertRaises(OSError):
                    socket.create_connection((parsed.hostname, parsed.port), timeout=2)

                http = urlsplit(opening('socket'))
                with socket.create_connection(('127.0.0.1', http.port), timeout=8) as ws:
                    origin = f'http://{http.netloc}'
                    ws.sendall(f'GET /rpc HTTP/1.1\r\nHost: {http.netloc}\r\nOrigin: {origin}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n'.encode())
                    head = b''
                    while not head.endswith(b'\r\n\r\n'):
                        part = ws.recv(1)
                        self.assertTrue(part, head)
                        head += part
                    self.assertTrue(head.startswith(b'HTTP/1.1 101'), head)
                    ws.sendall(b'\x82\x83\x01\x02\x03\x04\x01\xfd\x02')
                    frame = b''
                    while len(frame) < 5:
                        part = ws.recv(5-len(frame))
                        self.assertTrue(part, frame)
                        frame += part
                    self.assertEqual(frame, b'\x82\x03\x00\xff\x01')

                mutate({'op':'grant_service','service':'database','peer':pa})
                parsed = urlsplit(opening('database'))
                with socket.create_connection((parsed.hostname, parsed.port), timeout=8) as live:
                    live.sendall(b'alive');self.assertEqual(live.recv(5), b'alive')
                    a.request(op='stop')
                    self.assertEqual(live.recv(1), b'')
                with self.assertRaises(ValueError):
                    a.request(op='open', host=pb, service='database', incarnation=service['incarnation'])
                a.request(op='enable')
                parsed = urlsplit(opening('database'))
                with socket.create_connection((parsed.hostname, parsed.port), timeout=8) as live:
                    live.sendall(b'unpair');self.assertEqual(live.recv(6), b'unpair')
                    a.request(op='unpair', host=pb)
                    self.assertEqual(live.recv(1), b'')
                self.assertTrue(all(p.poll() is None for p in processes))
            finally:
                for proc in processes:
                    proc.terminate()
                for proc in processes:
                    try:
                        proc.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        proc.kill();proc.wait()
                echo.shutdown()


if __name__ == '__main__':
    unittest.main()
