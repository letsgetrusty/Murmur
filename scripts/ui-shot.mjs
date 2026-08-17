#!/usr/bin/env node
// ui-shot — capture the LIVE settings window (real WKWebView) per pane.
//
// The static ui-diff (scripts/ui-diff.mjs) renders in headless Chrome with the
// frontend JS stripped, so it can't see WebKit-specific rendering or anything
// the frontend JS populates (history rows, the speed segments, the mic label,
// live usage numbers, the native traffic lights / window chrome). This captures
// the actual app window instead, one PNG per pane, into docs/design/shots/.
//
// Usage:
//   node scripts/ui-shot.mjs                 # all panes
//   node scripts/ui-shot.mjs dictation read  # just these
//   node scripts/ui-shot.mjs --no-build      # skip the rebuild (reuse the bundle)
//   node scripts/ui-shot.mjs --out some/dir
//
// How it works (macOS, no window-automation entitlements needed):
//   1. `dev.sh --build-only` builds + signs the debug Murmur.app (the binary
//      embeds ../dist at compile time, so a rebuild is required to pick up any
//      frontend edit).
//   2. For each pane it launches the binary directly with MURMUR_UI_SHOT +
//      MURMUR_UI_PANE set. A debug build then opens the settings window straight
//      to that pane and writes its CGWindowID to MURMUR_UI_WINID_FILE (see
//      show_main_window in lib.rs).
//   3. `screencapture -l <windowid>` grabs exactly that window.
//
// Requires Screen Recording permission for the terminal (same as any
// screencapture). Debug-only hooks — the env vars do nothing in a release build.

import { execFileSync, spawn } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, rmSync, statSync } from "node:fs";
import { join, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { tmpdir } from "node:os";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const APP = join(ROOT, "src-tauri/target/debug/Murmur.app");
const BIN = join(APP, "Contents/MacOS/murmur");
const ALL = ["home", "dictation", "read", "shortcuts", "sound", "history", "support"];

// ---- args -----------------------------------------------------------------
const argv = process.argv.slice(2);
const flag = (name, def) => {
  const i = argv.indexOf(name);
  return i >= 0 ? argv[i + 1] : def;
};
const noBuild = argv.includes("--no-build");
const OUT = resolve(flag("--out", join(ROOT, "docs/design/shots")));
const panes = argv.filter((a) => ALL.includes(a));
const targets = panes.length ? panes : ALL;

const C = { dim: "\x1b[2m", green: "\x1b[32m", red: "\x1b[31m", bold: "\x1b[1m", reset: "\x1b[0m" };
const sh = (cmd, args, opts = {}) => execFileSync(cmd, args, { stdio: "inherit", ...opts });
const kill = () => {
  try {
    execFileSync("pkill", ["-f", "Murmur.app/Contents/MacOS/"], { stdio: "ignore" });
  } catch {
    /* nothing running */
  }
};
const sleep = (ms) => {
  // Synchronous sleep so the launch → windowid → capture sequence stays ordered
  // without threading async through the whole script. Atomics.wait blocks the
  // thread for `ms` with no external process and no busy-loop.
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
};

// ---- build ----------------------------------------------------------------
if (!noBuild) {
  console.log(`${C.dim}building signed debug bundle (dev.sh --build-only)…${C.reset}`);
  sh(join(ROOT, "scripts/dev.sh"), ["--build-only"]);
} else if (!existsSync(BIN)) {
  console.error(`${C.red}no bundle at ${BIN} — run without --no-build (or ./scripts/dev.sh) first${C.reset}`);
  process.exit(2);
}

mkdirSync(OUT, { recursive: true });

// ---- capture one pane -----------------------------------------------------
function capture(pane) {
  kill();
  sleep(600);
  const widFile = join(tmpdir(), `murmur-ui-winid-${pane}`);
  rmSync(widFile, { force: true });

  const child = spawn(BIN, [], {
    detached: true,
    stdio: "ignore",
    env: { ...process.env, MURMUR_UI_SHOT: "1", MURMUR_UI_PANE: pane, MURMUR_UI_WINID_FILE: widFile },
  });
  child.unref();

  // Wait for the app to create the window and drop its CGWindowID.
  let winid = null;
  for (let i = 0; i < 40; i++) {
    if (existsSync(widFile)) {
      winid = readFileSync(widFile, "utf8").trim();
      if (winid) break;
    }
    sleep(250);
  }
  if (!winid) {
    kill();
    throw new Error("window never reported its id (launch failed?)");
  }

  // Let the webview paint (fonts, JS-populated content) before the grab.
  sleep(1200);

  const out = join(OUT, `${pane}.png`);
  // -l <id>: capture that window · -o: omit the drop shadow · -x: no sound.
  execFileSync("screencapture", ["-l", winid, "-o", "-x", out], { stdio: "inherit" });
  kill();

  if (!existsSync(out) || statSync(out).size === 0) {
    throw new Error("screencapture produced no image (Screen Recording permission for the terminal?)");
  }
  return out;
}

// ---- run ------------------------------------------------------------------
console.log(`${C.dim}capturing ${targets.length} pane(s) → ${OUT.replace(ROOT + "/", "")}/${C.reset}\n`);
let failed = 0;
for (const pane of targets) {
  process.stdout.write(`  ${C.bold}${pane}${C.reset} … `);
  try {
    const out = capture(pane);
    const { width, height } = pngSize(out);
    console.log(`${C.green}✓${C.reset} ${C.dim}${width}×${height}${C.reset}`);
  } catch (e) {
    console.log(`${C.red}✗ ${e.message}${C.reset}`);
    failed++;
  }
}
kill();
console.log(
  `\n${failed === 0 ? C.green + `✓ ${targets.length} shot(s) in ${OUT.replace(ROOT + "/", "")}/` : C.red + `✗ ${failed} failed`}${C.reset}`
);
process.exit(failed === 0 ? 0 : 1);

// Read width/height from a PNG's IHDR (bytes 16–24) — avoids a dependency.
function pngSize(path) {
  const b = readFileSync(path);
  return { width: b.readUInt32BE(16), height: b.readUInt32BE(20) };
}
