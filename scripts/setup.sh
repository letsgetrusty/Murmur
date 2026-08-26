#!/usr/bin/env bash
#
# setup.sh — one-command onboarding for a fresh clone of Murmur.
#
# Gets a brand-new machine from `git clone` to "ready to run ./scripts/dev.sh":
#   1. checks the toolchain (Xcode CLT, Rust, Node),
#   2. installs frontend deps (npm install),
#   3. creates the self-signed "murmur dev" Code Signing cert and trusts it
#      (the CLI equivalent of the Keychain Access flow in
#      docs/macos-signing-and-permissions.md),
#   4. builds the debug binary and fetches the default local Whisper model.
#
# Everything runs on-device — no API keys, no accounts. Idempotent: every step
# checks-then-acts, so re-running is safe. The only step it can't do for you is
# granting Accessibility — it tells you exactly how.
#
# Usage: ./scripts/setup.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

IDENTITY="murmur dev"
BUNDLE_ID="dev.lgr.murmur"

say()  { printf '\033[1;36m▶ %s\033[0m\n' "$*"; }
ok()   { printf '\033[1;32m✓ %s\033[0m\n' "$*"; }
warn() { printf '\033[1;33m⚠ %s\033[0m\n' "$*"; }
die()  { printf '\033[1;31m✗ %s\033[0m\n' "$*" >&2; exit 1; }

# ver_ge A B → true if version A >= B
ver_ge() { [ "$(printf '%s\n%s\n' "$2" "$1" | sort -V | head -n1)" = "$2" ]; }

# ── 1. Toolchain preflight ──────────────────────────────────────────────────
say "Checking toolchain…"
[ "$(uname)" = "Darwin" ] || die "Murmur is macOS only."

xcode-select -p >/dev/null 2>&1 \
  || die "Xcode Command Line Tools not found. Install them with:  xcode-select --install"

command -v cargo >/dev/null 2>&1 \
  || die "Rust/cargo not found. Install rustup:  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
RUSTV="$(rustc --version | awk '{print $2}')"
ver_ge "$RUSTV" "1.77" || warn "Rust $RUSTV is below the 1.77 minimum — build may fail. rustup will honor rust-toolchain.toml on next build."

command -v node >/dev/null 2>&1 \
  || die "Node.js not found. Install Node 18+ (e.g. via nvm:  https://github.com/nvm-sh/nvm), then re-run."
NODEV="$(node -v | sed 's/^v//')"
ver_ge "$NODEV" "18.0.0" || warn "Node $NODEV is below 18 — Vite/Tauri may misbehave. See .nvmrc."

command -v npm >/dev/null 2>&1 || die "npm not found (ships with Node)."
ok "Toolchain OK — Rust $RUSTV, Node $NODEV"

# ── 2. Frontend dependencies ────────────────────────────────────────────────
if [ -d node_modules ]; then
  ok "node_modules present"
else
  say "Installing frontend deps (npm install)…"
  npm install
  ok "npm install done"
fi

# ── Git hooks ────────────────────────────────────────────────────────────────
# Enable the tracked pre-commit hook (rustfmt + clippy on Rust changes).
if [ "$(git config core.hooksPath 2>/dev/null)" = ".githooks" ]; then
  ok "Git hooks already enabled"
else
  git config core.hooksPath .githooks
  ok "Git hooks enabled (.githooks: pre-commit rustfmt + clippy)"
fi

# ── 3. Signing certificate ──────────────────────────────────────────────────
# A stable, self-signed "murmur dev" identity keeps the TCC designated
# requirement constant across rebuilds. See docs/macos-signing-and-permissions.md.
if security find-certificate -c "$IDENTITY" >/dev/null 2>&1; then
  ok "Signing cert '$IDENTITY' already in the login keychain"
else
  say "Creating self-signed Code Signing cert '$IDENTITY'…"
  LOGIN_KEYCHAIN="$(security default-keychain -d user | tr -d ' "')"
  TMP="$(mktemp -d)"
  trap 'rm -rf "$TMP"' EXIT   # scrub the private key material on exit

  cat > "$TMP/req.cnf" <<'CNF'
[ req ]
distinguished_name = dn
x509_extensions    = v3
prompt             = no
[ dn ]
CN = murmur dev
[ v3 ]
basicConstraints   = critical,CA:false
keyUsage           = critical,digitalSignature
extendedKeyUsage   = critical,codeSigning
CNF

  openssl req -x509 -newkey rsa:2048 -sha256 -days 3650 -nodes \
    -keyout "$TMP/key.pem" -out "$TMP/cert.pem" -config "$TMP/req.cnf" >/dev/null 2>&1
  # macOS's `security import` rejects OpenSSL 3's PKCS#12 defaults (SHA-256 MAC,
  # PBES2/AES-256-CBC) *and* rejects an empty password — both surface as the same
  # misleading "MAC verification failed ... (wrong password?)". Fixing only one
  # still fails (the message just changes to "Unknown format in import"), so
  # write a PKCS#12-era SHA-1/3DES bundle under a throwaway password. 3DES rather
  # than `-legacy`'s RC2-40: RC2 needs OpenSSL's legacy provider, 3DES doesn't,
  # so this also works under the system LibreSSL at /usr/bin/openssl.
  # The password protects nothing — the .p12 lives in $TMP, scrubbed on exit.
  P12PW="murmur-dev-import"
  openssl pkcs12 -export -inkey "$TMP/key.pem" -in "$TMP/cert.pem" \
    -name "$IDENTITY" -out "$TMP/cert.p12" -passout "pass:$P12PW" \
    -keypbe PBE-SHA1-3DES -certpbe PBE-SHA1-3DES -macalg sha1 >/dev/null 2>&1

  # -T /usr/bin/codesign lets codesign use the key; the partition list below
  # removes the per-signing "codesign wants to use key" prompt (needs the
  # login-keychain password).
  security import "$TMP/cert.p12" -k "$LOGIN_KEYCHAIN" -P "$P12PW" \
    -T /usr/bin/codesign -T /usr/bin/security >/dev/null
  ok "Cert imported into $LOGIN_KEYCHAIN"

  printf '   To skip the per-run "codesign wants to sign" popup, enter your login\n'
  printf '   (keychain) password, or press Enter to skip and click "Always Allow"\n'
  printf '   once the first time dev.sh signs.\n'
  read -rs -p "   Login keychain password (optional): " KCPW; echo
  if [ -n "$KCPW" ]; then
    if security set-key-partition-list -S apple-tool:,apple: -s -k "$KCPW" "$LOGIN_KEYCHAIN" >/dev/null 2>&1; then
      ok "codesign pre-authorized (no popup on signing)"
    else
      warn "Couldn't set the partition list — you'll click 'Always Allow' once on first sign. Harmless."
    fi
    unset KCPW
  else
    warn "Skipped — click 'Always Allow' on the first Keychain prompt when dev.sh signs."
  fi
fi

# Trust the cert for code signing (privileged, one-time). Trusting makes TCC
# match on the stable designated requirement instead of the per-build cdhash,
# so the Accessibility grant survives rebuilds.
if security dump-trust-settings -d 2>/dev/null | grep -qi "$IDENTITY"; then
  ok "Cert already trusted for code signing"
else
  say "Trusting the cert for code signing (needs sudo — writes a trusted root to the System keychain)…"
  PEM="$(mktemp -t murmurdev)"
  security find-certificate -c "$IDENTITY" -p > "$PEM"
  sudo security add-trusted-cert -d -r trustRoot -p codeSign \
    -k /Library/Keychains/System.keychain "$PEM"
  rm -f "$PEM"
  ok "Cert trusted"
fi

# ── 4. Build ─────────────────────────────────────────────────────────────────
say "Building the debug binary…"
cargo build --manifest-path src-tauri/Cargo.toml --no-default-features
ok "Build OK"

# ── Local Whisper model ──────────────────────────────────────────────────────
# Dictation defaults to on-device Whisper (whisper-rs). Fetch the default model
# now so the first dictation works offline instead of waiting on the download.
MODEL_NAME="small.en"
MODELS_DIR="$HOME/Library/Application Support/murmur/models"
MODEL_FILE="$MODELS_DIR/ggml-$MODEL_NAME.bin"
if [ -f "$MODEL_FILE" ]; then
  ok "Local Whisper model '$MODEL_NAME' already present"
else
  say "Downloading local Whisper model '$MODEL_NAME' (~466 MB, one-time)…"
  mkdir -p "$MODELS_DIR"
  if curl -fL --progress-bar -o "$MODEL_FILE.part" \
      "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-$MODEL_NAME.bin"; then
    mv "$MODEL_FILE.part" "$MODEL_FILE"
    ok "Local Whisper model ready"
  else
    rm -f "$MODEL_FILE.part"
    warn "Model download failed — Murmur will retry it on first launch."
  fi
fi

ok "Everything runs on-device — no API keys or accounts to configure."

# ── 5. Done ─────────────────────────────────────────────────────────────────
echo
ok "Setup complete."
printf '\nNext:\n'
printf '  1. Run:  \033[1m./scripts/dev.sh\033[0m\n'
printf '  2. Grant Accessibility once: System Settings → Privacy & Security →\n'
printf '     Accessibility → enable \033[1mMurmur\033[0m, then re-run ./scripts/dev.sh.\n'
printf '  3. Triggers: hold Fn to dictate · Cmd+Shift+R read-aloud.\n'
printf '\nLogs: tail -f ~/Library/Logs/murmur.log\n'
