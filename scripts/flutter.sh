#!/usr/bin/env bash
set -euo pipefail
if [[ -n "${FLUTTER_ROOT:-}" && -x "$FLUTTER_ROOT/bin/flutter" ]]; then
  exec "$FLUTTER_ROOT/bin/flutter" "$@"
elif command -v flutter >/dev/null 2>&1; then
  exec flutter "$@"
elif [[ -x "$HOME/.local/share/flutter/bin/flutter" ]]; then
  exec "$HOME/.local/share/flutter/bin/flutter" "$@"
else
  echo 'Flutter 3.47.9 or newer is required. Install the stable SDK and set FLUTTER_ROOT or PATH.' >&2
  exit 1
fi
