#!/usr/bin/env bash
# Test the current source snapshot in a disposable Linux environment.
set -euo pipefail
cd "$(dirname "$0")/.."
runtime="${WISPNET_CONTAINER_RUNTIME:-docker}"
image=wispnet-client-tests:local
snapshot=$(mktemp -d)
trap 'rm -rf "$snapshot"' EXIT
# Include current tracked edits and non-ignored new files, without personal .git
# config, ignored build output, credentials or a sibling server checkout.
git ls-files --cached --others --exclude-standard -z |
  tar --null -T - -cf - | tar -xf - -C "$snapshot"
"$runtime" build -t "$image" -f scripts/testing/Dockerfile scripts/testing
# Only the source snapshot is exposed. No host HOME, SSH agent, devices, network
# namespace or Wispnet state is mounted; build artifacts die with the container.
"$runtime" run --rm --init -i "$image" bash -euo pipefail -c '
  tar --no-same-owner -xf - -C /workspace
  git init -q -b main
  git config user.name "Disposable test fixture"
  git config user.email "fixture@example.invalid"
  git add .
  git commit -qm "Source snapshot"
  ./scripts/install-hooks.sh
  exec ./scripts/test.sh
' < <(tar -cf - -C "$snapshot" .)
