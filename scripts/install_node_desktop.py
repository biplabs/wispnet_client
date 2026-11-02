#!/usr/bin/env python3
"""Install the existing node, with desktop/tray only in a graphical session."""

import argparse
from pathlib import Path
import os
import filecmp
import shutil
import subprocess
import time
import tempfile
import hashlib
import json
import sys

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument(
    "--no-start",
    action="store_true",
    help="Install files only; do not enable, start or restart services",
)
parser.add_argument(
    "--headless",
    action="store_true",
    help="Install only the node; skip desktop launcher and tray",
)
parser.add_argument("--configure-firewall", action="store_true",
                    help="Request one-time administrator installation of the UDP firewall helper")
args = parser.parse_args()
if args.configure_firewall and args.no_start:
    parser.error("--configure-firewall cannot be combined with --no-start")

ROOT = Path(__file__).resolve().parents[1]
node_source = ROOT / "bin/wispnet"
if node_source.exists():
    # Distribution checks happen before changing any installed file or service.
    manifest = json.loads((ROOT / "manifest.json").read_text())
    for name, expected in manifest["sha256"].items():
        path = ROOT / name
        if (
            Path(name).is_absolute()
            or ".." in Path(name).parts
            or path.is_symlink()
            or hashlib.sha256(path.read_bytes()).hexdigest() != expected
        ):
            raise SystemExit(f"Bundle checksum mismatch: {name}")
else:
    node_source = ROOT / "target/debug/wispnet"
if sys.version_info < (3, 11):
    raise SystemExit("Python 3.11 or newer is required")
subprocess.run([str(node_source), "--help"], check=True, stdout=subprocess.DEVNULL)
# Validate/build the replacement before changing the installed node or desktop.
graphical = (
    not args.headless
    and subprocess.run(
        ["systemctl", "--user", "is-active", "--quiet", "graphical-session.target"]
    ).returncode
    == 0
)
desktop_source = ROOT / "bin/Wispnet"
if graphical and not desktop_source.exists():
    build_script = ROOT / "scripts/build_flutter_desktop.sh"
    if not build_script.exists():
        raise SystemExit("This is a headless bundle. Install a desktop bundle to run the GUI.")
    subprocess.run([str(build_script)], check=True)
    desktop_source = ROOT / "apps/flutter/build/linux/x64/release/bundle/Wispnet"
home = Path.home()
if args.configure_firewall:
    helper = ROOT / "scripts/install_firewall_helper.py"
    command = [sys.executable, str(helper), "--uid", str(os.getuid())]
    if os.geteuid() != 0:
        command.insert(0, "pkexec" if graphical and shutil.which("pkexec") else "sudo")
    subprocess.run(command, check=True)

# Upgrade the misspelled installation without moving or recreating identity/state.
# The copied configuration deliberately retains its original state_dir.
legacy_config = home / ".config/wipsnet-node/host.toml"
config = home / ".config/wispnet-node/host.toml"
if legacy_config.exists() and not config.exists():
    config.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    config.parent.chmod(0o700)
    with config.open("x") as destination_config:
        config.chmod(0o600)
        destination_config.write(legacy_config.read_text())
    for helper in ["admin-targets.json"]:
        legacy_helper = legacy_config.parent / helper
        if legacy_helper.exists():
            target_helper = config.parent / helper
            with target_helper.open("x") as output_helper:
                target_helper.chmod(0o600)
                output_helper.write(legacy_helper.read_text())
units = home / ".config/systemd/user"
units.mkdir(parents=True, exist_ok=True)
node_unit = units / "wispnet-host.service"
node_unit_source = ROOT / "deploy/node/wispnet-host.service"
node_unit_changed = not node_unit.exists() or not filecmp.cmp(
    node_unit_source, node_unit, shallow=False
)
shutil.copy2(node_unit_source, node_unit)
node_binary = home / ".local/libexec/wispnet-node/wispnet"
node_binary.parent.mkdir(parents=True, exist_ok=True)
node_changed = not node_binary.exists() or not filecmp.cmp(
    node_source, node_binary, shallow=False
)
if node_changed:
    node_temporary = node_binary.with_name("wispnet.installing")
    shutil.copy2(node_source, node_temporary)
    node_temporary.chmod(0o755)
    os.replace(node_temporary, node_binary)
cli_launcher = home / ".local/bin/wispnet"
cli_launcher.parent.mkdir(parents=True, exist_ok=True)
cli_temporary = cli_launcher.with_name("wispnet.installing")
cli_temporary.unlink(missing_ok=True)
cli_temporary.symlink_to(node_binary)
os.replace(cli_temporary, cli_launcher)
# The headless command shares the existing node API and needs only Python stdlib.
api_package = node_binary.parent / "wispnet_desktop"
api_package.mkdir(exist_ok=True)
for name in ["__init__.py", "api.py", "combined.py", "catalog_cache.py"]:
    shutil.copy2(ROOT / "apps/node/desktop/wispnet_desktop" / name, api_package / name)
shutil.copy2(ROOT / "scripts/node_request.py", node_binary.parent / "node_request.py")
shutil.copy2(ROOT / "scripts/mcp_headers.py", node_binary.parent / "mcp_headers.py")
node_launcher = home / ".local/bin/wispnet-node"
node_launcher.parent.mkdir(parents=True, exist_ok=True)
node_launcher.write_text(
    "#!/usr/bin/env python3\nimport os,sys\nos.execv(sys.executable, [sys.executable, "
    + repr(str(node_binary.parent / "node_request.py"))
    + ",*sys.argv[1:]])\n"
)
node_launcher.chmod(0o755)
if not args.no_start:
    legacy_unit = units / "wipsnet-host.service"
    if legacy_unit.exists():
        subprocess.run(
            ["systemctl", "--user", "disable", "--now", "wipsnet-host.service"],
            check=True,
        )
        legacy_unit.unlink()
    subprocess.run(["systemctl", "--user", "daemon-reload"], check=True)
    subprocess.run(
        ["systemctl", "--user", "enable", "wispnet-host.service"], check=True
    )
if not args.no_start and (
    node_changed
    or node_unit_changed
    or subprocess.run(
        ["systemctl", "--user", "is-active", "--quiet", "wispnet-host.service"]
    ).returncode
):
    subprocess.run(
        ["systemctl", "--user", "restart", "wispnet-host.service"], check=True
    )
if not args.no_start:
    deadline = time.monotonic() + 10
    while True:
        ready = subprocess.run(
            [str(node_launcher)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            timeout=30,
        )
        if ready.returncode == 0:
            break
        if time.monotonic() >= deadline:
            raise RuntimeError(
                "Installed node did not become ready; inspect wispnet-host.service"
            )
        time.sleep(0.1)
    # Remove only the obsolete executable after the replacement answers IPC.
    (home / ".local/libexec/wipsnet-node/wipsnet").unlink(missing_ok=True)
# A headless installation never starts a display-dependent service.
if not graphical:
    print(
        "Installed headless node; desktop/tray skipped. Identity/configuration/grants preserved."
    )
    raise SystemExit(0)

destination = home / ".local/libexec/wispnet-desktop"
destination.mkdir(parents=True, exist_ok=True)
shutil.copy2(ROOT / "apps/node/desktop/icon.png", destination / "icon.png")
temporary_desktop = destination / "Wispnet.installing"
if (desktop_source.parent / "lib/libflutter_linux_gtk.so").exists():
    # Stage the complete engine/assets bundle before switching the launcher.
    # Running processes keep their old runtime until the service restart succeeds.
    runtime = Path(tempfile.mkdtemp(prefix="flutter-", dir=destination))
    shutil.copytree(desktop_source.parent, runtime, dirs_exist_ok=True)
    (runtime / "python-path").write_text(str(Path(sys.executable).resolve()))
    temporary_desktop.unlink(missing_ok=True)
    temporary_desktop.symlink_to(runtime / "Wispnet")
else:
    shutil.copy2(desktop_source, temporary_desktop)
    temporary_desktop.chmod(0o755)
if (destination / "Wispnet").is_file() and not (destination / "Wispnet").is_symlink():
    shutil.copy2(destination / "Wispnet", destination / "Wispnet.previous")
os.replace(temporary_desktop, destination / "Wispnet")
shutil.copy2(ROOT / "apps/flutter/desktop/desktop_data.py", destination / "desktop_data.py")
(destination / "python-path").write_text(str(Path(sys.executable).resolve()))
shutil.copytree(api_package, destination / "wispnet_desktop", dirs_exist_ok=True)
binpath = home / ".local/bin/wispnet-desktop"
binpath.parent.mkdir(parents=True, exist_ok=True)
temporary_launcher = binpath.with_suffix(".installing")
temporary_launcher.unlink(missing_ok=True)
temporary_launcher.symlink_to(destination / "Wispnet")
os.replace(temporary_launcher, binpath)
applications = home / ".local/share/applications"
applications.mkdir(parents=True, exist_ok=True)
(applications / "wispnet.desktop").write_text(
    '[Desktop Entry]\nType=Application\nName=Wispnet\nComment=Your services and connected devices\nExec="'
    + str(binpath)
    + '" %u\nIcon='
    + str(destination / "icon.png")
    + "\nCategories=Network;\nTerminal=false\nMimeType=x-scheme-handler/wispnet;\n"
)
shutil.copy2(
    ROOT / "deploy/node/wispnet-desktop.service", units / "wispnet-desktop.service"
)
# Make the custom scheme discoverable to browser/desktop launchers immediately.
if updater := shutil.which("update-desktop-database"):
    subprocess.run([updater, str(applications)], check=True)
if not args.no_start and (mime := shutil.which("xdg-mime")):
    subprocess.run([mime, "default", "wispnet.desktop", "x-scheme-handler/wispnet"], check=True)

if args.no_start:
    print("Installed desktop files; services were not enabled or started.")
    raise SystemExit(0)
subprocess.run(["systemctl", "--user", "daemon-reload"], check=True)
subprocess.run(["systemctl", "--user", "enable", "wispnet-desktop.service"], check=True)
subprocess.run(
    ["systemctl", "--user", "restart", "wispnet-desktop.service"], check=True
)
time.sleep(2)
subprocess.run(
    ["systemctl", "--user", "is-active", "--quiet", "wispnet-desktop.service"],
    check=True,
)
# The native replacement is running; retire only Wispnet's former Qt runtime.
for name in [".venv", "pyproject.toml", "uv.lock", "wispnet_desktop/app.py",
             "wispnet_desktop/icon.png", "wispnet_desktop/build_info.py",
             "wispnet_desktop/__pycache__"]:
    legacy = destination / name
    if legacy.is_dir() and not legacy.is_symlink():
        shutil.rmtree(legacy)
    else:
        legacy.unlink(missing_ok=True)
print(
    "Installed Flutter desktop and session tray; identity/configuration/grants preserved."
)
