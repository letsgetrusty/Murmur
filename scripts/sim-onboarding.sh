#!/usr/bin/env bash
# Simulate a brand-new Murmur install to test the first-run onboarding flow —
# the Accessibility + Microphone grants and the one-time model downloads —
# WITHOUT uninstalling or downloading the prod build. Fully reversible: your real
# state is *moved* aside, never deleted.
#
#   scripts/sim-onboarding.sh reset                # become a new user + relaunch (onboarding shows)
#   scripts/sim-onboarding.sh reset --keep-models  # skip the ~1.5 GB re-download (reuse what's
#                                                  #   already downloaded) for fast iteration
#   scripts/sim-onboarding.sh restore              # put your real config/history/models back
#   scripts/sim-onboarding.sh status               # is a sim currently active?
#
# How it works:
#   • First `reset` moves your real per-user state (config, history, usage, and
#     the models dir) into $BAK, so the app starts truly fresh and the onboarding
#     download step runs for real.
#   • `reset` is repeatable and, by DEFAULT, reproduces a genuine first-run every
#     time — it clears the onboarding/permission state AND the sim's downloaded
#     models, so the download step runs live (~1.5 GB). Pass --keep-models when
#     you're iterating and don't want to re-fetch them.
#   • It resets the Accessibility + Microphone TCC grants each time, so you
#     re-grant them exactly like a new user (and can test the acceptsFirstMouse
#     fix: after granting Accessibility you return to an inactive window, and the
#     Enable-microphone button must still fire on the first click).
#   • `restore` puts everything back. You'll re-grant Accessibility once afterward
#     (TCC was reset) so Fn works again in your normal dev build.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP="$HOME/Library/Application Support/murmur"
BAK="$HOME/.murmur-onboarding-sim"
BUNDLE_ID="dev.lgr.murmur"
# Per-user state a fresh install wouldn't have. dev-terms.tab is bundled (the app
# rewrites it on launch), so it's intentionally left in place.
STATE=(config.json history.db usage.json models)
EPHEMERAL=(config.json history.db usage.json) # cheap to recreate; refreshed every reset

quit_murmur() {
  osascript -e 'quit app "Murmur"' 2>/dev/null || true
  pkill -x murmur 2>/dev/null || true
  sleep 1
}

reset_tcc() {
  for svc in Accessibility Microphone ListenEvent; do
    tccutil reset "$svc" "$BUNDLE_ID" >/dev/null 2>&1 && echo "  reset TCC: $svc" || true
  done
}

relaunch() { ( cd "$REPO" && ./scripts/dev.sh ); }

case "${1:-}" in
  reset)
    quit_murmur
    if [ ! -d "$BAK" ]; then
      # First reset: preserve the real state (including models → forces the live
      # download step, like a genuine new install).
      mkdir -p "$BAK"
      for item in "${STATE[@]}"; do
        [ -e "$APP/$item" ] && mv "$APP/$item" "$BAK/$item" && echo "  preserved real: $item"
      done
      echo "  (real state saved in $BAK — 'restore' brings it back)"
    else
      # Already in a sim: restart the flow from a clean slate. Clear the
      # onboarding/permission state AND the sim's downloaded models by default,
      # so every pass reproduces a true first-run (live model downloads). Pass
      # --keep-models to reuse what's already downloaded for fast iteration.
      for item in "${EPHEMERAL[@]}"; do
        [ -e "$APP/$item" ] && rm -rf "$APP/$item"
      done
      if [ "${2:-}" = "--keep-models" ]; then
        echo "  keeping already-downloaded models (--keep-models)"
      else
        rm -rf "$APP/models" && echo "  cleared sim models — the download step runs again (~1.5 GB)"
      fi
    fi
    reset_tcc
    echo "✓ Fresh-user state. Relaunching…"
    relaunch
    echo "→ Onboarding should appear. Grant Accessibility + Microphone as prompted."
    ;;

  restore)
    if [ ! -d "$BAK" ]; then
      echo "✗ No sim backup at $BAK — nothing to restore (not in a sim)." >&2
      exit 1
    fi
    quit_murmur
    for item in "${STATE[@]}"; do
      [ -e "$APP/$item" ] && rm -rf "$APP/$item" # drop the throwaway sim copy
      [ -e "$BAK/$item" ] && mv "$BAK/$item" "$APP/$item" && echo "  restored: $item"
    done
    rmdir "$BAK" 2>/dev/null || true
    echo "✓ Real state restored. Relaunching…"
    relaunch
    echo "→ Re-grant Accessibility in System Settings (TCC was reset) so Fn works again."
    ;;

  status)
    if [ -d "$BAK" ]; then
      echo "SIM ACTIVE — real state is parked in $BAK. Run 'restore' to return to normal."
      ls -1 "$BAK" 2>/dev/null | sed 's/^/  parked: /'
    else
      echo "Not in a sim — Murmur is running on your real state."
    fi
    ;;

  *)
    echo "usage: scripts/sim-onboarding.sh {reset [--keep-models]|restore|status}" >&2
    exit 2
    ;;
esac
