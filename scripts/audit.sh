#!/usr/bin/env bash
#
# audit.sh — check dependencies against the RustSec advisory database.
#
# Deliberately NOT in the pre-commit hook: the result depends on the dependency
# tree and the advisory DB (network), not on your edits, so per-commit runs are
# pure latency. Run this on demand — especially after changing dependencies:
#   ./scripts/audit.sh
#
# Installs cargo-audit on first use. Any extra args pass through to cargo-audit
# (e.g. `./scripts/audit.sh --deny warnings`).

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../src-tauri"

if ! command -v cargo-audit >/dev/null 2>&1; then
  printf '\033[1;33m▶ cargo-audit not found — installing (one-time)…\033[0m\n'
  # --locked uses cargo-audit's pinned deps, so its install isn't broken by a
  # transitive crate that outpaces our pinned toolchain (rust-toolchain.toml).
  cargo install --locked cargo-audit
fi

exec cargo audit "$@"
