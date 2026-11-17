#!/usr/bin/env python3
"""Opt-in installed-node security acceptance; no GUI on the selected remote, no relay restart.

Requires existing reciprocal approvals, local Chromium/uv Playwright, and existing
headless Firefox/geckodriver on the selected remote. Temporary services/units are removed.
"""

import json
import shlex
from pathlib import Path
import subprocess
import sys
import time
import uuid

from test_physical_service_recreation import (
    configure,
    execute,
    mutate,
    request,
    snapshot,
    stable,
    http,
    restart,
    SSH,
)

ROOT = Path(__file__).resolve().parents[1]
RAW = r"""import http.client,json,socket,sys,urllib.parse
v=json.load(sys.stdin);u=urllib.parse.urlsplit(v['url']);origin='http://'+u.netloc
cases=[('POST','/',{'Origin':'https://unlisted.example'},200),('POST','/',{},403),('GET','/',{'Origin':'null'},200),('GET','/',{'Sec-Fetch-Site':'cross-site'},403),('GET','/',{'Host':'evil.localhost:'+str(u.port)},421),('GET','/sw.js',{'Service-Worker':'script'},403),('TRACE','/',{},405),('POST','/',{'Origin':origin,'Content-Length':'16777217'},413),('GET','/',{'Transfer-Encoding':'chunked'},400)]
for method,path,extra,status in cases:
 c=http.client.HTTPConnection('127.0.0.1',u.port,timeout=15);h={'Host':u.netloc,'Cookie':v['cookie']};h.update(extra)
 c.request(method,path,headers=h);r=c.getresponse();assert r.status==status,(method,status,r.status);r.read();c.close()
c=http.client.HTTPConnection('127.0.0.1',u.port,timeout=15)
c.request('POST','/_wispnet/session',headers={'Host':u.netloc,'Origin':origin,'Content-Length':'0'})
r=c.getresponse();assert r.status==404,'retired app handoff endpoint';r.read();c.close()
# Actual non-loopback interface, not merely a different Host header.
s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM);s.connect(('192.0.2.1',9));ip=s.getsockname()[0];s.close();assert not ip.startswith('127.')
s=socket.socket();s.settimeout(2)
try:s.connect((ip,u.port));raise AssertionError('gateway reachable on LAN interface')
except (ConnectionRefusedError,TimeoutError):pass
finally:s.close()
print('raw CSRF/null/cross-site/Host/SW/method/upload/framing/bootstrap replay denied; LAN socket inaccessible')
"""


def main():
    args = configure()
    baseline = {r: snapshot(r) for r in [False, True]}
    name = "gateway-security-" + uuid.uuid4().hex[:10]
    dirs = {}
    relay = execute(
        True,
        [
            "systemctl",
            "--user",
            "show",
            "wispnet-relay.service",
            "-p",
            "MainPID",
            "--value",
        ],
    ).stdout.strip()
    try:
        for remote in [False, True]:
            dirs[remote] = execute(
                remote, ["mktemp", "-d", "/tmp/wispnet-security-XXXXXXXX"]
            ).stdout.strip()
            for script in ["gateway_security_fixture.py", "gateway_browser_probe.py"]:
                if remote:
                    subprocess.run(
                        [
                            "scp",
                            "-q",
                            str(ROOT / "scripts" / script),
                            args.ssh_host + ":" + dirs[remote] + "/" + script,
                        ],
                        check=True,
                    )
                else:
                    import shutil

                    shutil.copy2(ROOT / "scripts" / script, dirs[remote] + "/" + script)
            execute(
                remote,
                [
                    "systemd-run",
                    "--user",
                    "--quiet",
                    "--collect",
                    "--unit=" + name,
                    "python3",
                    dirs[remote] + "/gateway_security_fixture.py",
                    "--ready",
                    dirs[remote] + "/port",
                    "--label",
                    "remote" if remote else "local",
                ],
            )
            for _ in range(50):
                result = execute(remote, ["cat", dirs[remote] + "/port"], check=False)
                if result.returncode == 0:
                    break
                time.sleep(0.1)
            port = int(result.stdout)
            mutate(
                remote,
                {
                    "op": "service",
                    "id": name,
                    "backend": "127.0.0.1:" + str(port),
                    "allow": [baseline[not remote]["host"]["host"]],
                    "display_name": "<script>malicious metadata</script>",
                },
            )
            request(remote, {"op": "enable"})
        for source in [False, True]:
            urls = []
            for target in [False, True]:
                host = baseline[target]["host"]["host"]
                catalog = (
                    {"services": snapshot(source)["host"]["services"]}
                    if source == target
                    else request(source, {"op": "catalog", "host": host})
                )
                service = next(s for s in catalog["services"] if s["id"] == name)
                assert service["display_name"] == service["id"]
                urls.append(
                    request(
                        source,
                        {
                            "op": "open",
                            "host": host,
                            "service": name,
                            "incarnation": service["incarnation"],
                        },
                    )["url"]
                )
            command = (
                ["python3", dirs[source] + "/gateway_browser_probe.py"]
                if source
                else [
                    "uv",
                    "run",
                    "--with",
                    "playwright",
                    "python",
                    dirs[source] + "/gateway_browser_probe.py",
                ]
            )
            # Browser subprocess may take up to 60s; URLs never enter argv/logs.
            result = execute(
                source,
                command,
                json.dumps(
                    {"browser": "firefox" if source else "chromium", "urls": urls}
                ),
                check=False,
            )
            if result.returncode:
                raise RuntimeError("Browser probe failed: " + result.stderr[-1800:])
            print(result.stdout.strip(), flush=True)
            for destination in [False, True]:
                address = next(
                    s["backend"]
                    for s in snapshot(destination)["host"]["services"]
                    if s["id"] == name
                )
                count = "import urllib.request,sys;print(urllib.request.urlopen('http://'+sys.argv[1]+'/attacks',timeout=5).read().decode())"
                assert (
                    execute(
                        destination, ["python3", "-c", count, address]
                    ).stdout.strip()
                    == "0"
                ), "cross-origin browser attack reached backend"
            print(
                "Cross-origin fetch, WebSocket, iframe GET and form POST: zero backend requests",
                flush=True,
            )

            target = not source
            host = baseline[target]["host"]["host"]
            service = next(
                s
                for s in request(source, {"op": "catalog", "host": host})["services"]
                if s["id"] == name
            )
            opening = {
                "op": "open",
                "host": host,
                "service": name,
                "incarnation": service["incarnation"],
            }
            url = request(source, opening)["url"]
            cookie = http(source, url)["cookie"]
            print(
                execute(
                    source,
                    ["python3", "-c", RAW],
                    json.dumps({"url": url, "cookie": cookie}),
                ).stdout.strip(),
                flush=True,
            )
            # Interrupt a write after the backend commits; fresh Open must not replay it.
            post_code = """import http.client,json,sys,urllib.parse
v=json.load(sys.stdin);u=urllib.parse.urlsplit(v['url']);c=http.client.HTTPConnection('127.0.0.1',u.port,timeout=10)
try:
 c.request('POST','/write',body=b'',headers={'Host':u.netloc,'Origin':'http://'+u.netloc,'Cookie':v['cookie']});c.getresponse().read()
except (OSError,http.client.HTTPException):pass
finally:c.close()
"""
            post_args = ["python3", "-c", post_code]
            proc = subprocess.Popen(
                SSH + [shlex.join(post_args)] if source else post_args,
                stdin=subprocess.PIPE,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                text=True,
            )
            proc.stdin.write(json.dumps({"url": url, "cookie": cookie}))
            proc.stdin.close()
            backend_port = next(
                s["backend"]
                for s in snapshot(target)["host"]["services"]
                if s["id"] == name
            ).rsplit(":", 1)[1]
            count_code = "import urllib.request,sys;print(urllib.request.urlopen('http://127.0.0.1:'+sys.argv[1]+'/writes',timeout=5).read().decode())"
            deadline = time.monotonic() + 10
            while (
                execute(
                    target, ["python3", "-c", count_code, backend_port]
                ).stdout.strip()
                != "1"
            ):
                assert time.monotonic() < deadline, "backend write not received"
                time.sleep(0.1)
            request(source, {"op": "stop"})
            proc.wait(timeout=12)
            request(source, {"op": "enable"})
            url = request(source, opening)["url"]
            cookie = http(source, url)["cookie"]
            assert http(source, url, cookie)["status"] == 200
            time.sleep(0.2)
            assert (
                execute(
                    target, ["python3", "-c", count_code, backend_port]
                ).stdout.strip()
                == "1"
            ), "write replayed"
            print(
                "Interrupted application POST committed exactly once; fresh Open did not replay it",
                flush=True,
            )
            # Host-local grant revocation survives a browser/client node restart.
            mutate(
                target,
                {
                    "op": "service",
                    "id": name,
                    "backend": next(
                        s["backend"]
                        for s in snapshot(target)["host"]["services"]
                        if s["id"] == name
                    ),
                    "allow": [],
                },
            )
            assert http(source, url, cookie)["status"] == 403
            restart(source)
            request(source, {"op": "enable"})
            request(source, opening, ok=False)
            mutate(
                target,
                {
                    "op": "service",
                    "id": name,
                    "backend": next(
                        s["backend"]
                        for s in snapshot(target)["host"]["services"]
                        if s["id"] == name
                    ),
                    "allow": [baseline[source]["host"]["host"]],
                },
            )
            request(source, {"op": "catalog", "host": host})
            fresh = request(source, opening)["url"]
            fresh_cookie = http(source, fresh)["cookie"]
            assert http(source, fresh, fresh_cookie)["status"] == 200
            print(
                ("remote host → local" if source else "local → remote host")
                + ": host-local revoke denied old route and survived client restart; explicit fresh Open restored access",
                flush=True,
            )
    finally:
        for remote in [False, True]:
            if any(s["id"] == name for s in snapshot(remote)["host"]["services"]):
                mutate(remote, {"op": "remove", "id": name})
            request(
                remote,
                {"op": "enable" if baseline[remote]["access_enabled"] else "stop"},
            )
            execute(remote, ["systemctl", "--user", "stop", name], check=False)
            if remote in dirs:
                execute(remote, ["rm", "-rf", dirs[remote]])
            assert stable(snapshot(remote)) == stable(baseline[remote]), (
                "authority or pins changed"
            )
    assert (
        execute(
            True,
            [
                "systemctl",
                "--user",
                "show",
                "wispnet-relay.service",
                "-p",
                "MainPID",
                "--value",
            ],
        ).stdout.strip()
        == relay
    )
    assert (
        execute(
            True,
            [
                "systemctl",
                "--user",
                "show",
                "wispnet-desktop.service",
                "-p",
                "LoadState",
                "--value",
            ],
        ).stdout.strip()
        == "not-found"
    )
    print(
        "Temporary services/units removed, authority/pins/access restored, relay PID unchanged; the selected remote remained headless"
    )


if __name__ == "__main__":
    main()
