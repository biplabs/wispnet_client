#!/usr/bin/env python3
"""Private stdio adapter for the shared Flutter UI; reuses the installed node API.

No listener, key import or alternate identity. Cached data is display-only.
"""
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
import errno
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import sqlite3
import stat
import subprocess
import sys
import tempfile
import threading
import time
import tomllib
from urllib.parse import urlsplit

# Keep the installed, signed application bundle unchanged when importing helpers.
sys.dont_write_bytecode = True

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[2]
sys.path.insert(0, str(ROOT / 'apps/node/desktop') if (ROOT / 'apps/node/desktop').exists() else str(HERE))
sys.path.insert(0, str(HERE))
from wispnet_desktop.api import NodeAPI, NodeRejected
from wispnet_desktop.catalog_cache import CatalogCache
from wispnet_desktop.combined import Combined
from desktop_data import dispatch as desktop_data


def write_private(path, value):
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    fd, temporary = tempfile.mkstemp(dir=path.parent, prefix='.flutter-')
    try:
        with os.fdopen(fd, 'w') as output:
            json.dump(value, output)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
    finally:
        Path(temporary).unlink(missing_ok=True)


def browser_url(url):
    parsed = urlsplit(url)
    if (parsed.scheme != 'http' or not (parsed.hostname or '').endswith('.localhost')
        or len(parsed.hostname) <= 10 or not parsed.port
        or not (parsed.path == '/' or (parsed.hostname == 'registry.wispnet.localhost' and parsed.path == '/v1/services') or (parsed.hostname == 'wispnet.localhost' and parsed.path == '/registry'))
        or parsed.query or parsed.fragment or parsed.username or parsed.password
        or any(ord(c) < 33 for c in url)):
        raise ValueError('Invalid local browser handoff')
    return url


def public_browser_url(url):
    if (not isinstance(url, str) or len(url) > 2048
        or any(ord(c) < 33 or 127 <= ord(c) <= 159 for c in url)):
        raise ValueError('Invalid public browser handoff')
    try:
        parsed = urlsplit(url)
        host, port = parsed.hostname, parsed.port
    except ValueError:
        raise ValueError('Invalid public browser handoff') from None
    if (parsed.scheme != 'https' or not host or len(host) > 253
        or not all(re.fullmatch(r'[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?', label, re.I)
                   for label in host.split('.'))
        or port not in (None, 443) or parsed.netloc.endswith(':')
        or parsed.path != '/' or '?' in url or '#' in url
        or parsed.username is not None or parsed.password is not None):
        raise ValueError('Invalid public browser handoff')
    return url


def readable_device_id(value):
    if re.fullmatch(r'[a-z0-9][a-z0-9._-]{0,63}', value):
        return value
    result = ''
    for c in value.lower():
        if c.isascii() and (c.isalnum() or result and c in '._-'):
            result += c
        elif result and not result.endswith('-'):
            result += '-'
        if len(result) == 64:
            break
    return result.rstrip('._-') or 'device'


def launch_terminal(command, state):
    """Launch the native terminal with a fixed argv, keeping protocol bytes out of pipes."""
    fd, name = tempfile.mkstemp(prefix='shell-', suffix='.command', dir=state)
    try:
        with os.fdopen(fd, 'w') as output:
            output.write('#!/bin/sh\nrm -f -- "$0"\nexec ' + shlex.join(command) + '\n')
        os.chmod(name, 0o700)
        if sys.platform == 'darwin':
            subprocess.run(['/usr/bin/open', '-a', 'Terminal', name], check=True, timeout=15,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            return
        # Qt parses its own --session option even after Konsole's -e. Keep
        # Wispnet's arguments inside the private script, away from the terminal.
        for terminal, options in (
            ('x-terminal-emulator', ['-e']),
            ('gnome-terminal', ['--']),
            ('konsole', ['-e']),
            ('xfce4-terminal', ['-x']),
            ('kitty', []),
            ('alacritty', ['-e']),
            ('xterm', ['-e']),
        ):
            executable = shutil.which(terminal)
            if executable:
                subprocess.Popen([executable, *options, '/bin/sh', name], stdin=subprocess.DEVNULL,
                                 stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                 start_new_session=True, close_fds=True)
                return
        raise ValueError('Install a terminal application, or connect with wispnet shell from a terminal')
    except BaseException:
        Path(name).unlink(missing_ok=True)
        raise


class Bridge:
    def __init__(self, config=None):
        self.config = Path(config or Path.home() / '.config/wispnet-node/host.toml')
        self.api = NodeAPI(self.config)
        self.cache = CatalogCache(self.api.state)
        self.lock = threading.RLock()
        self.records = {}
        self.status = None
        self.path = self.api.state / 'desktop-flutter.json'
        self.preferences = desktop_data({'op': 'preferences'}, self.config)
        self.display = {}
        if self.path.exists():
            saved = Combined.read_private(self.path)
            self.preferences = saved.get('preferences', self.preferences)
            self.display = saved.get('display', {})
        else:
            self.migrate_webkit()
        self.preferences.pop('names', None)
        for row in self.display.get('hosts', {}).values():
            if 'name' in row:
                row['name'] = readable_device_id(row['name'])
        if 'name' in self.display:
            self.display['name'] = readable_device_id(self.display['name'])

    def migrate_webkit(self):
        # Read only; never alter the previous runtime's database or WAL.
        directory = Path(os.environ.get('XDG_DATA_HOME', Path.home() / '.local/share'))
        candidates = [directory / 'app.wispnet.desktop/localstorage/tauri_localhost_0.localstorage',
                      Path.home() / 'Library/WebKit/app.wispnet.desktop/WebsiteData/LocalStorage/tauri_localhost_0.localstorage']
        for path in candidates:
            if not path.is_file():
                continue
            try:
                with closing(sqlite3.connect(path.as_uri() + '?mode=ro', uri=True)) as db:
                    for key, raw in db.execute('SELECT key,value FROM ItemTable'):
                        if key != 'cardOrder':
                            continue
                        value = json.loads(raw.decode('utf-16-le') if isinstance(raw, bytes) else raw)
                        if isinstance(value, dict):
                            self.preferences['order'].update(value)
            except (sqlite3.Error, ValueError, UnicodeError):
                # Existing private preferences remain usable if WebKit storage is unavailable.
                continue

    def save(self):
        with self.lock:
            display = {**self.display, 'hosts': {peer: {k: v for k, v in row.items() if k in ('name', 'local')} for peer, row in self.display.get('hosts', {}).items()}}
            write_private(self.path, {'preferences': self.preferences, 'display': display})

    def lifecycle(self, enabled):
        expected = Path.home() / '.config/wispnet-node/host.toml'
        if self.config.resolve() != expected.resolve():
            raise ValueError('Lifecycle requires the installed node configuration')
        if sys.platform == 'darwin':
            domain = f'gui/{os.getuid()}'
            service = domain + '/app.wispnet.node'
            loaded = subprocess.run(['/bin/launchctl', 'print', service], capture_output=True, timeout=30).returncode == 0
            command = (['/bin/launchctl', 'kickstart', service] if loaded else
                       ['/bin/launchctl', 'bootstrap', domain, str(Path.home() / 'Library/LaunchAgents/app.wispnet.node.plist')]) if enabled else ['/bin/launchctl', 'bootout', service]
            if not enabled and not loaded:
                return
        else:
            command = ['systemctl', '--user', 'start' if enabled else 'stop', 'wispnet-host.service']
        result = subprocess.run(command, capture_output=True, text=True, timeout=30)
        if result.returncode:
            raise ValueError(result.stderr.strip() or 'Cannot change node state')
        if enabled:
            deadline = time.monotonic() + 12
            while True:
                try:
                    self.api.request(op='status')
                    return
                except (OSError, ValueError):
                    if time.monotonic() >= deadline:
                        raise ValueError('Node started but its API is not ready')
                    time.sleep(.1)

    def snapshot(self):
        try:
            status = self.api.request(op='status', refresh_clients=True)
        except OSError as error:
            if error.errno not in (errno.ENOENT, errno.ECONNREFUSED):
                raise
            with self.lock:
                return {**self.display, 'enabled': False, 'desktop': True,
                        'order': self.preferences['order'], 'hosts': self.display.get('hosts', {}),
                        'network': 'Node offline', 'version': self.version()}
        # A restarted node has a fresh gateway. Restore saved listeners before
        # reporting online, including when the desktop itself stayed running.
        if not status.get('access_enabled'):
            self.api.request(op='enable')
        with self.lock:
            self.status = status
            own = status['host']['host']
            device_ids = self.preferences.setdefault('device_ids', {})
            publications = {p['service']: p for p in status.get('public_access', {}).get('services', [])}
            owned_services = []
            for service in status['host']['services']:
                if service.get('kind') == 'files' and not status['host'].get('file_exports'):
                    continue
                item = dict(service)
                publication = publications.get(service['id'])
                if publication and publication.get('incarnation') == service.get('incarnation'):
                    item['public_access'] = dict(publication)
                owned_services.append(item)
            hosts = {own: {'name': status['host_name'], 'state': 'available', 'status': 'This machine',
                          'local': True, 'catalog': {'services': owned_services}}}
            clients = {c['peer']: c for c in status['host']['clients']}
            for peer in dict.fromkeys([*status['peer_pins'], *clients]):
                if peer == own:
                    continue
                record = dict(self.records.get(peer, {}))
                if not record:
                    try:
                        cached = self.cache.load(peer)
                    except (OSError, ValueError):
                        # Display caches are disposable; a bad entry must not hide
                        # other machines or prevent a fresh owner-authenticated fetch.
                        cached = None
                    record = {'state': 'unverified', 'status': 'Cached; refresh required',
                              'catalog': cached['catalog'] if cached else {'services': []}}
                record['name'] = device_ids.get(peer) or readable_device_id(clients.get(peer, {}).get('name') or 'device-' + peer[:8])
                hosts[peer] = record
            self.display = {'name': status['host_name'], 'peer': own, 'hosts': hosts,
                            'relay': status['relay'], 'telemetry': {'peers': status['peer_telemetry']}}
            self.save()
            return {**self.display, 'enabled': True, 'desktop': True, 'order': self.preferences['order'],
                    'network': 'Network available', 'version': self.version(),
                    'pendingRequests': sum(i['state'] == 'pending' for i in status['invitations']),
                    'activeClients': sum(status.get('connected_peers', {}).get(c['peer'], 0) > 0 or c['peer'] in status.get('recent_peers', {}) for c in status['host']['clients'])}

    def version(self):
        path = HERE / 'build-identity'
        return path.read_text().strip() if path.exists() else 'development'

    def refresh(self, peer):
        with self.lock:
            if peer == self.display.get('peer'):
                return {}
            if peer not in self.display.get('hosts', {}):
                raise ValueError('Machine was removed')
        try:
            catalog = self.api.request(op='catalog', host=peer)
            if catalog['host'] != peer:
                raise ValueError('Wrong catalog host')
            self.cache.save(peer, {'catalog': catalog, 'fetched': time.time()})
            record = {'state': 'available', 'status': 'Available', 'catalog': catalog}
            # Optional display metadata: old peers may not support this request.
            try:
                links = self.api.request(op='host_public_links', host=peer)
                if links.get('version') == 1 and links.get('host') == peer and isinstance(links.get('services'), list):
                    record['public_access'] = links
            except (OSError, ValueError):
                pass
            # Optional on older hosts; catalog permission checks remain independent.
            try:
                device_id = self.api.request(op='host_device_id', host=peer)['device_id']
                if not re.fullmatch(r'[a-z0-9][a-z0-9._-]{0,63}', device_id):
                    raise ValueError('Invalid device ID')
                with self.lock:
                    self.preferences.setdefault('device_ids', {})[peer] = device_id
            except (OSError, ValueError, KeyError):
                pass
            try:
                self.api.request(op='set_name', host=peer, name=self.display['name'])
            except (OSError, ValueError):
                pass
        except (OSError, ValueError) as error:
            text = str(error)
            with self.lock:
                previous = self.records.get(peer, self.display.get('hosts', {}).get(peer, {}))
            record = {**previous, 'state': 'host_denied' if 'forbidden' in text.lower() or 'denied' in text.lower() else 'unreachable', 'status': text}
        with self.lock:
            if peer in self.display.get('hosts', {}):
                self.records[peer] = record
        return {}

    def open_shell(self, args):
        host, service, incarnation = (args.get(k) for k in ('host', 'service', 'incarnation'))
        session = args.get('session', 'main')
        if (not isinstance(host, str) or not re.fullmatch(r'[0-9a-f]{64}', host)
            or not isinstance(incarnation, str) or not re.fullmatch(r'[0-9a-f]{64}', incarnation)
            or not isinstance(service, str) or not re.fullmatch(r'[a-z0-9][a-z0-9._-]{0,63}', service)
            or not isinstance(session, str) or not re.fullmatch(r'[a-z0-9][a-z0-9._-]{0,63}', session)):
            raise ValueError('Invalid shell session')
        self.api.request(op='enable')
        status = self.api.request(op='status')
        catalog = status['host'] if status['host']['host'] == host else self.api.request(op='catalog', host=host)
        if not any(s['id'] == service and s.get('kind') == 'shell' and s.get('incarnation') == incarnation
                   for s in catalog['services']):
            raise ValueError('Shell service unavailable or no longer permitted; refresh this machine')
        installed = Path.home() / '.local/libexec/wispnet-node/wispnet'
        source = ROOT / 'target/debug/wispnet'
        executable = str(installed) if installed.is_file() else shutil.which('wispnet')
        if not executable and source.is_file():
            executable = str(source)
        if not executable:
            raise ValueError('Wispnet CLI unavailable; install the current Wispnet node')
        launch_terminal([executable, 'shell', '--host', host, '--service', service,
                         '--incarnation', incarnation, '--session', session,
                         '--config', str(self.config.resolve())], self.api.state)
        return {}

    def file_target(self, args):
        host, incarnation = (args.get(key) for key in ('host', 'incarnation'))
        selected = args.get('service', 'files')
        if (not isinstance(host, str) or not re.fullmatch(r'[0-9a-f]{64}', host)
            or not isinstance(incarnation, str) or not re.fullmatch(r'[0-9a-f]{64}', incarnation)
            or not isinstance(selected, str) or not re.fullmatch(r'[a-z0-9][a-z0-9._-]{0,63}', selected)):
            raise ValueError('Invalid Files target; refresh this machine')
        status = self.api.request(op='status')
        catalog = status['host'] if status['host']['host'] == host else self.api.request(op='catalog', host=host)
        if (catalog.get('host') != host or not any(
                service.get('id') == selected and service.get('kind') in ('files', 'file_inbox')
                and service.get('incarnation') == incarnation
                and service.get('enabled') is not False
                for service in catalog.get('services', []))):
            raise ValueError('Files access unavailable or changed; refresh this machine')
        return dict(host=host, service=selected, incarnation=incarnation)

    def send_file(self, args):
        workspace = args.get('workspace', '')
        if not isinstance(workspace, str) or workspace and not re.fullmatch(r'[a-z0-9][a-z0-9._-]{0,63}', workspace):
            raise ValueError('Choose a receiving folder')
        local_path, remote_path = args.get('local_path'), args.get('remote_path')
        if (not isinstance(local_path, str) or '\0' in local_path
            or not Path(local_path).is_absolute() or not Path(local_path).name):
            raise ValueError('Choose an absolute local file path')
        source = Path(local_path)
        if not stat.S_ISREG(source.lstat().st_mode):
            raise ValueError('Choose regular files; folders and symbolic links are not supported')
        # Resolve parent aliases while retaining the final entry for the node's
        # O_NOFOLLOW check. File content and hashing remain inside the node.
        source = source.parent.resolve(strict=True) / source.name
        if (not isinstance(remote_path, str) or not remote_path or '\0' in remote_path
            or '\\' in remote_path or len(remote_path.encode('utf-8')) > 4096
            or any(not part or part in ('.', '..') or part.startswith('.wispnet-')
                   or len(part.encode('utf-8')) > 255 for part in remote_path.split('/'))):
            raise ValueError('Destination must be a relative file path inside the receiving folder')
        resume = args.get('resume', True)
        if type(resume) is not bool:
            raise ValueError('Resume must be true or false')
        target = self.file_target(args)
        return self.api.request(op='file_put', **target, workspace=workspace,
                                local_path=str(source), remote_path=remote_path, resume=resume)

    def share_target(self, args):
        host, service, incarnation = (args.get(key) for key in ('host', 'service', 'incarnation'))
        if (not isinstance(host, str) or not re.fullmatch(r'[0-9a-f]{64}', host)
            or not isinstance(incarnation, str) or not re.fullmatch(r'[0-9a-f]{64}', incarnation)
            or not isinstance(service, str) or not re.fullmatch(r'[a-z0-9][a-z0-9._-]{0,63}', service)):
            raise ValueError('Invalid shared files target; refresh this machine')
        status = self.api.request(op='status')
        catalog = status['host'] if status['host']['host'] == host else self.api.request(op='catalog', host=host)
        if catalog.get('host') != host or not any(
                entry.get('id') == service and entry.get('kind') == 'file_share'
                and entry.get('incarnation') == incarnation and entry.get('enabled') is not False
                for entry in catalog.get('services', [])):
            raise ValueError('Shared files access unavailable or changed; refresh this machine')
        return dict(host=host, service=service, incarnation=incarnation)

    def shared_files(self, method, args):
        path = args.get('path', '')
        if (not isinstance(path, str) or '\0' in path or '\\' in path
            or len(path.encode('utf-8')) > 4096 or path and any(
                not part or part in ('.', '..') or part.startswith('.wispnet-')
                or len(part.encode('utf-8')) > 255 for part in path.split('/'))):
            raise ValueError('Choose a relative path inside the share')
        target = self.share_target(args)
        if method == 'shareList':
            cursor = args.get('cursor')
            if cursor is not None and (not isinstance(cursor, str) or len(cursor.encode('utf-8')) > 4096):
                raise ValueError('Invalid directory cursor')
            return self.api.request(op='file_list', **target, path=path, cursor=cursor)
        destination = args.get('local_path')
        if (not isinstance(destination, str) or '\0' in destination
            or not Path(destination).is_absolute() or not Path(destination).name):
            raise ValueError('Choose an absolute download destination')
        resume = args.get('resume', True)
        if type(resume) is not bool:
            raise ValueError('Resume must be true or false')
        recursive = args.get('recursive', False)
        if type(recursive) is not bool:
            raise ValueError('Recursive must be true or false')
        if not path and not recursive:
            raise ValueError('Choose a file or download the whole folder')
        return self.api.request(op='file_get', **target, path=path,
                                local_path=destination, resume=resume,
                                **({'recursive': True} if recursive else {}))

    def dispatch(self, method, args):
        if method == 'sshStatus':
            host = args.get('host', '')
            if not isinstance(host, str) or not re.fullmatch(r'[0-9a-f]{64}', host):
                raise ValueError('Invalid Shell host')
            executable = Path.home() / '.local/libexec/wispnet-node/wispnet'
            installed = False
            if executable.is_file():
                result = subprocess.run([str(executable), 'ssh-bridge', '--help'],
                                        capture_output=True, text=True, timeout=5)
                installed = result.returncode == 0 and '--stdio' in result.stdout
            command = None
            path = Path.home() / '.ssh/config'
            try:
                fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
                with os.fdopen(fd) as config:
                    meta = os.fstat(config.fileno())
                    if not stat.S_ISREG(meta.st_mode) or meta.st_uid != os.geteuid() or meta.st_mode & 0o022 or meta.st_size > 1024 * 1024:
                        raise ValueError('Cannot verify SSH alias: unsafe or oversized configuration')
                    text = config.read(1024 * 1024 + 1)
                begin, end = f'# Wispnet bridge {host} begin\n', f'# Wispnet bridge {host} end\n'
                if begin in text and end in text.split(begin, 1)[1]:
                    block = text.split(begin, 1)[1].split(end, 1)[0]
                    alias = re.search(r'^Host (shell-[a-z0-9._-]+\.localhost)$', block, re.MULTILINE)
                    if alias and f'--host {host}' in block and 'ssh-bridge' in block:
                        command = 'ssh ' + alias.group(1)
            except FileNotFoundError:
                pass
            return {'installed': installed, 'alias_configured': command is not None, 'command': command}
        if method == 'sshSetup':
            host = args.get('host', '')
            if not isinstance(host, str) or not re.fullmatch(r'[0-9a-f]{64}', host):
                raise ValueError('Invalid Shell host')
            executable = Path.home() / '.local/libexec/wispnet-node/wispnet'
            result = subprocess.run([str(executable), 'ssh-bridge', '--host', host,
                                     '--config', str(self.config.resolve()), '--install-config'],
                                    capture_output=True, text=True, timeout=35)
            if result.returncode:
                raise ValueError(result.stderr.strip() or 'Could not add SSH bridge')
            return {'command': result.stdout.strip()}
        if method == 'startDesktop':
            self.lifecycle(True)
            self.api.request(op='enable')
            return {}
        if method == 'snapshot':
            return self.snapshot()
        if method == 'resolveLink':
            return self.api.request(op='resolve_link', link=args['link'])
        if method == 'takeInvitation':
            return {}
        if method == 'setAccess':
            self.lifecycle(args['enabled'])
            if args['enabled']:
                self.api.request(op='enable')
            return {}
        if method == 'refreshHost':
            return self.refresh(args['host'])
        if method == 'connectionDiagnostics':
            host = args.get('host')
            if not isinstance(host, str) or not re.fullmatch(r'[0-9a-f]{64}', host):
                raise ValueError('Choose a paired device to diagnose')
            status = self.api.request(op='status')
            paired = {*status.get('peer_pins', []),
                      *(client['peer'] for client in status['host']['clients'])}
            if host == status['host']['host'] or host not in paired:
                raise ValueError('Choose a paired remote device to diagnose')
            row = {'host': host,
                   'name': self.preferences.get('device_ids', {}).get(host) or
                           self.records.get(host, {}).get('name') or
                           next((client.get('name') for client in status['host']['clients']
                                 if client['peer'] == host), None) or host[:8]}
            try:
                row['report'] = self.api.request(op='diagnose_connection', host=host)
            except (OSError, ValueError) as error:
                row['error'] = str(error)
            # The desktop adapter has no portable network-type/VPN observer.
            # Report these fields as unavailable rather than inferring them from
            # the diagnostic path or the operating-system name.
            return {'network': None, 'vpn': None, 'network_changed': None,
                    'peers': [row]}
        if method in ('preview', 'pair'):
            code = re.sub(r'\s+', '', args['code'])
            if not code.startswith('wispnet1:') or len(code.encode()) > 8192:
                raise ValueError('Paste a complete Wispnet pairing code')
            self.lifecycle(True)
            preview = self.api.request(op='preview_invitation', invitation=code)
            if method == 'preview':
                return preview
            result = self.api.request(op='pair', host=preview['host'], invitation=code)
            with self.lock:
                self.preferences.setdefault('device_ids', {})[preview['host']] = readable_device_id(preview.get('host_name') or 'device-' + preview['host'][:8])
                self.records[preview['host']] = {'state': 'approval_pending', 'status': 'Pending owner approval'}
                self.save()
            return result
        if method == 'open':
            self.api.request(op='enable')
            result = self.api.request(op='open', host=args['host'], service=args['service'], incarnation=args['incarnation'],
                                      host_name=self.preferences.get('device_ids', {}).get(args['host']))
            # Both platforms launch the stable service address directly.
            return {'url': browser_url(result['url'])}
        if method == 'openShell':
            return self.open_shell(args)
        if method == 'fileWorkspaces':
            return self.api.request(op='file_workspaces', **self.file_target(args))
        if method == 'fileSend':
            return self.send_file(args)
        if method in ('shareList', 'shareGet'):
            return self.shared_files(method, args)
        if method in ('fileStatus', 'fileCancel'):
            transfer = args.get('transfer')
            if not isinstance(transfer, str) or not re.fullmatch(r'[0-9a-f]{64}', transfer):
                raise ValueError('Invalid file transfer handle')
            return self.api.request(op='file_status' if method == 'fileStatus' else 'file_cancel', transfer=transfer)
        if method == 'shellOpen':
            self.api.request(op='enable')
            return self.api.request(
                op='shell_open',
                host=args['host'],
                service=args.get('service', 'shell'),
                incarnation=args['incarnation'],
                session=args.get('session', 'main'),
                cols=int(args.get('cols', 80)),
                rows=int(args.get('rows', 24)),
            )
        if method == 'shellPoll':
            return self.api.request(op='shell_poll', session=args['session'])
        if method == 'shellInput':
            return self.api.request(op='shell_input', session=args['session'], data_b64=args['data_b64'])
        if method == 'shellResize':
            return self.api.request(
                op='shell_resize',
                session=args['session'],
                cols=int(args['cols']),
                rows=int(args['rows']),
            )
        if method == 'shellClose':
            return self.api.request(
                op='shell_close',
                session=args['session'],
                terminate=bool(args.get('terminate', False)),
            )
        if method == 'launchPublic':
            status = self.api.request(op='status')
            publication = next((p for p in status.get('public_access', {}).get('services', [])
                                if p['service'] == args['service'] and p.get('enabled') is True), None)
            if not publication or not isinstance(publication.get('url'), str):
                raise ValueError('Public URL unavailable')
            url = public_browser_url(publication['url'])
            subprocess.run(['/usr/bin/open' if sys.platform == 'darwin' else 'xdg-open', url],
                           check=True, timeout=15, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            return {'url': url}
        if method == 'launch':
            if not isinstance(args['url'], str):
                raise ValueError('Invalid browser handoff')
            try:
                url = browser_url(args['url'])
            except ValueError:
                url = public_browser_url(args['url'])
            subprocess.run(['/usr/bin/open' if sys.platform == 'darwin' else 'xdg-open', url],
                           check=True, timeout=15, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            return {}
        if method == 'removeHost':
            status = self.api.request(op='status')
            if args['host'] == status['host']['host']:
                raise ValueError('This is the local machine; manage its shared services in Settings')
            self.api.request(op='unpair', host=args['host'], expected_revision=status['host']['revision'])
            with self.lock:
                self.records.pop(args['host'], None)
                self.preferences.get('device_ids', {}).pop(args['host'], None)
                self.display.get('hosts', {}).pop(args['host'], None)
                self.cache.remove(args['host'])
                self.save()
            return {}
        if method == 'saveOrder':
            group, ids = args['group'], args['ids']
            if not (group == 'machines' or re.fullmatch(r'services/[0-9a-f]{64}', group)) or not isinstance(ids, list) or len(ids) > (128 if group == 'machines' else 162) or not all(isinstance(i, str) for i in ids):
                raise ValueError('Invalid card order')
            with self.lock:
                self.preferences['order'][group] = ids
                self.save()
            return {}
        if method in ('settings', 'saveSettings'):
            data = self.api.request(op='gateway_settings')
            settings = data['settings']
            host, service = args.get('host'), args.get('service')
            if method == 'settings':
                if host is None:
                    return {'port': settings['registry_port'], 'url': f"http://wispnet.localhost:{settings['registry_port']}/registry"}
                route = next((p for p in data['routes'] if p['host'] == host and p['service'] == service), {})
                return {'port': settings['registry_port'], 'url': route.get('url', '')}
            port = args['port']
            if type(port) is not int or not 1024 <= port <= 65535:
                raise ValueError('Port must be between 1024 and 65535')
            if host is None:
                name = args['name'].strip()
                if not re.fullmatch(r'[a-z0-9][a-z0-9._-]{0,63}', name):
                    raise ValueError('Invalid device ID')
            settings['registry_port'] = port
            settings['service_ports'] = []
            self.api.request(op='configure_access', settings=settings)
            if host is None and name != self.api.request(op='status')['host_name']:
                self.rename_device(name)
            return {}
        if method == 'setServiceEnabled':
            if type(args.get('enabled')) is not bool:
                raise ValueError('Invalid service state')
            status = self.api.request(op='status')
            return self.api.request(op='mutate', operation={
                'op': 'set_service_enabled',
                'expected_revision': status['host']['revision'],
                'id': args['service'], 'incarnation': args['incarnation'],
                'enabled': args['enabled'],
            })
        if method in ('desktopStatus', 'ownerStatus'):
            return self.api.request(op='status', refresh_clients=True)
        if method in ('desktopRequest', 'ownerRequest'):
            payload = args['payload']
            if payload.get('op') not in ('mutate', 'invite_qr', 'configure_public'):
                raise ValueError('Unsupported owner operation')
            return self.api.request(**payload)
        if method in ('desktopData', 'ownerData'):
            return {'value': desktop_data(args, self.config)}
        raise ValueError('Unsupported desktop UI operation')

    def rename_device(self, name):
        if self.config.resolve() != (Path.home() / '.config/wispnet-node/host.toml').resolve():
            raise ValueError('Rename requires the installed node configuration')
        text = self.config.read_text()
        section = re.search(r'^\s*\[', text, re.M)
        index = section.start() if section else len(text)
        top, rest = text[:index], text[index:]
        top = re.sub(r'^(?:device_name|device_id)\s*=.*\n?', '', top, flags=re.M)
        updated = 'device_id=' + json.dumps(name, ensure_ascii=True) + '\n' + top + rest
        tomllib.loads(updated)
        fd, temporary = tempfile.mkstemp(dir=self.config.parent, prefix='.rename-')
        try:
            with os.fdopen(fd, 'w') as out:
                out.write(updated)
                out.flush()
                os.fsync(out.fileno())
            os.replace(temporary, self.config)
        finally:
            Path(temporary).unlink(missing_ok=True)
        self.lifecycle(False)
        self.lifecycle(True)


def main():
    bridge = Bridge()
    output_lock = threading.Lock()
    def respond(payload):
        try:
            value = {'id': payload['id'], 'result': bridge.dispatch(payload['method'], payload.get('arguments', {}))}
        except Exception as error:
            value = {'id': payload.get('id'), 'error': str(error)}
        with output_lock:
            print(json.dumps(value), flush=True)
    with ThreadPoolExecutor(max_workers=6) as executor:
        while True:
            line = sys.stdin.buffer.readline(65537)
            if not line:
                break
            if len(line) > 65536:
                raise ValueError('Oversized desktop request')
            executor.submit(respond, json.loads(line))


if __name__ == '__main__':
    main()
