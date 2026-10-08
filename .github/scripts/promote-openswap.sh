#!/usr/bin/env bash
# Commit only the dependency files that passed every compatibility gate.
set -euo pipefail

git fetch origin main
current="$(git show origin/main:Cargo.toml | python3 .github/scripts/pin-openswap.py revision)"
if [[ "$current" != "$OPENSWAP_SHA" ]]; then
  status="$(gh api "repos/citadel-foss/openswap/compare/${OPENSWAP_SHA}...${current}" --jq .status)"
  if [[ "$status" == ahead ]]; then
    echo "A newer compatible OpenSwap commit is already pinned; leaving it in place." >> "$GITHUB_STEP_SUMMARY"
    exit 0
  fi
  if [[ "$status" != behind ]]; then
    echo "::error::The current OpenSwap pin is not an ancestor of the tested commit."
    exit 1
  fi
fi
if git diff --quiet -- Cargo.toml Cargo.lock; then
  echo "The selected OpenSwap commit is already pinned." >> "$GITHUB_STEP_SUMMARY"
  exit 0
fi
if [[ "$(git rev-parse origin/main)" != "$GITHUB_SHA" ]]; then
  echo "::error::BTCPay plugin main changed during the build. Re-run this workflow on the latest main before updating the pin."
  exit 1
fi
git config user.name 'github-actions[bot]'
git config user.email '41898282+github-actions[bot]@users.noreply.github.com'
git add Cargo.toml Cargo.lock
git commit -m "chore: pin OpenSwap to ${OPENSWAP_SHA}"
# A normal push rejects races rather than overwriting concurrent main changes.
# The checkout's GITHUB_TOKEN credentials do not trigger another push workflow.
git push origin HEAD:main
printf 'Pinned OpenSwap to `%s` in Cargo.toml and Cargo.lock.\n' "$OPENSWAP_SHA" >> "$GITHUB_STEP_SUMMARY"
