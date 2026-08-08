#!/usr/bin/env bash
#
# npm-audit.sh — audit the npm dependency tree, tuned so it's a useful CI gate
# rather than perma-red on accepted dev-tool noise.
#
# Two tiers:
#   1. SHIPPED deps (production): zero tolerance. Nothing that reaches an end
#      user may carry a known vulnerability. The Tauri app embeds the built
#      frontend, so only `dependencies` ship — `devDependencies` (vite, vitest,
#      esbuild, …) never do.
#   2. DEV deps: block only CRITICAL. The dev toolchain currently carries known
#      moderate/high advisories in vite/esbuild that are only fixable by a
#      breaking vite major bump and affect only the local dev server, never the
#      shipped app or CI (which runs `vitest run`, no server). Those are
#      consciously accepted (see AGENTS.md); a CRITICAL still fails so a genuine
#      supply-chain problem in the toolchain can't slip through.
#
# Run from anywhere; operates on the repo root.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

printf '\033[1;36m▶ npm audit: shipped (production) deps — zero tolerance\033[0m\n'
npm audit --omit=dev --audit-level=low

printf '\033[1;36m▶ npm audit: full tree — CRITICAL blocks (dev moderate/high accepted)\033[0m\n'
npm audit --audit-level=critical

printf '\033[1;36m▶ npm audit: full report (informational)\033[0m\n'
npm audit || true

printf '\033[1;32m▶ npm audit: OK\033[0m\n'
