#!/usr/bin/env python3
"""Real pacman/netfilter lifecycle in a disposable Arch container.

Systemd calls are recorded by a stub: this does not claim a real boot test.
Never mounts the host's /etc, /home, /run, Docker socket or network namespace.
"""
import argparse
from pathlib import Path
import subprocess
import uuid

COMMANDS = r'''
set -eu
pacman -Syu --noconfirm --needed gtk3 libayatana-appindicator xdg-utils diffutils >/tmp/dependencies.log 2>&1
# No systemd manager runs in this container. Record service lifecycle requests.
cp /usr/bin/systemctl /usr/bin/systemctl.real
cat > /usr/bin/systemctl <<'SH'
#!/bin/bash
printf '%s\n' "$*" >> /tmp/systemctl.log
for arg in "$@"; do
    if [ "$arg" = is-active ]; then exit 3; fi
    if [ "$arg" = show ]; then
        unit="${!#}"
        printf '/usr/lib/systemd/user/%s\n' "$unit"
        exit 0
    fi
    if [ "$arg" = restart ] && [ "${!#}" = wispnet-firewall.service ]; then
        mkdir -p /run/wispnet-firewall
        printf '{"version":1,"healthy":true,"uids":[1000],"ports":[],"updated":%s,"error":null}\n' "$(date +%s)" > /run/wispnet-firewall/status.json
    fi
done
exit 0
SH
chmod 755 /usr/bin/systemctl
touch /tmp/systemctl.log
chmod 666 /tmp/systemctl.log
mkdir -p /run/user/1000/systemd
useradd -m -u 1000 aurtest
touch /run/user/1000/systemd/private
pacman -U --noconfirm /packages/*.pkg.tar.zst >/tmp/install.log 2>&1
/usr/bin/wispnet --help >/dev/null
ldd /opt/wispnet/bin/Wispnet | tee /tmp/ldd.log
! grep 'not found' /tmp/ldd.log
runuser -u aurtest -- wispnet-setup --headless >/tmp/setup.log 2>&1
runuser -u aurtest -- wispnet host-status --config /home/aurtest/.config/wispnet-node/host.toml > /tmp/before.json
sha256sum /home/aurtest/.config/wispnet-node/host.toml > /tmp/config.sha
# Exercise the native package firewall administrator. The systemctl stub
# publishes readiness because this container has no systemd manager.
wispnet-setup --firewall-uid 1000
# Seed managed rules as a real running helper would; removal must validate their
# exact ownership markers and preserve all unrelated INPUT rules.
for table in iptables ip6tables; do
    "$table" -N WISPNET-UDP
    "$table" -A WISPNET-UDP -p udp -m udp --dport 41088 -m comment --comment wispnet-uid-1000 -j ACCEPT
    "$table" -I INPUT 1 -j WISPNET-UDP
done
! command -v python3
! find /opt/wispnet -name '*.py' -print -quit | grep .
iptables -A INPUT -p tcp --dport 22 -j ACCEPT
# Upgrade/reinstall must retain firewall authorization and identity.
pacman -U --noconfirm /packages/*.pkg.tar.zst >/tmp/upgrade.log 2>&1
test -f /etc/wispnet-firewall.json
iptables -C WISPNET-UDP -p udp -m udp --dport 41088 -m comment --comment wispnet-uid-1000 -j ACCEPT
runuser -u aurtest -- wispnet-setup --headless >> /tmp/setup.log 2>&1
runuser -u aurtest -- wispnet host-status --config /home/aurtest/.config/wispnet-node/host.toml > /tmp/after.json
cmp /tmp/before.json /tmp/after.json
sha256sum -c /tmp/config.sha
# A foreign rule must abort the transaction rather than be deleted.
iptables -A WISPNET-UDP -j DROP
if pacman -R --noconfirm wispnet-bin >/tmp/rejected-remove.log 2>&1; then
    echo 'Expected cleanup failure to abort removal' >&2; exit 1
fi
pacman -Q wispnet-bin
iptables -D WISPNET-UDP -j DROP
pacman -R --noconfirm wispnet-bin >/tmp/remove.log 2>&1
! pacman -Q wispnet-bin
! test -e /opt/wispnet
! test -e /etc/wispnet-firewall.json
! test -e /usr/lib/systemd/system/wispnet-firewall.service
! iptables -S WISPNET-UDP
! ip6tables -S WISPNET-UDP
iptables -C INPUT -p tcp --dport 22 -j ACCEPT
sha256sum -c /tmp/config.sha
test -d /home/aurtest/.local/state/wispnet-node
grep -- '--user --machine aurtest@.host stop wispnet-host.service' /tmp/systemctl.log
grep -- 'disable --now wispnet-firewall.service' /tmp/systemctl.log
echo 'Real pacman install/upgrade/remove, failed-cleanup abort, firewall cleanup and identity preservation passed.'
'''


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--package',type=Path,required=True)
    args=parser.parse_args()
    package=args.package.resolve()
    name='wispnet-aur-test-'+uuid.uuid4().hex[:10]
    try:
        subprocess.run(['docker','run','-d','--name',name,'--cap-add','NET_ADMIN',
                        '-v',str(package)+':/packages/wispnet.pkg.tar.zst:ro','archlinux:base','sleep','infinity'],check=True)
        result=subprocess.run(['docker','exec','-i',name,'bash'],input=COMMANDS,text=True)
        if result.returncode:
            subprocess.run(['docker','exec',name,'bash','-c',
                            'for f in /tmp/*.log; do echo "$f"; tail -25 "$f"; done'])
            raise SystemExit(result.returncode)
    finally:
        subprocess.run(['docker','rm','-f',name],check=False,stdout=subprocess.DEVNULL)


if __name__=='__main__':main()
