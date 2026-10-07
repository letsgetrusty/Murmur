#!/usr/bin/env bash
# Local production-engine benchmark gate; see docs/benchmarks.md.
set -euo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
exec python3 "$REPO/scripts/bench-speech.py" "$@"
