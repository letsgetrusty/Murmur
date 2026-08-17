#!/usr/bin/env node
// ui-shot — capture the LIVE app windows (real WKWebView), one PNG per section.
//
// The static ui-diff (scripts/ui-diff.mjs) renders in headless Chrome with the
// frontend JS stripped, so it can't see WebKit-specific rendering or anything
// the frontend JS populates (history rows, speed segments, download %, live
// permission badges, the native traffic lights / window chrome). This captures
// the actual app windows instead, into docs/design/shots/. Two targets:
//   settings    — the 7 settings panes         → settings-<pane>.png
//   onboarding  — the 6 onboarding steps        → onboarding-<step>.png
//
// Usage:
//   node scripts/ui-shot.mjs                     # both windows, every section
//   node scripts/ui-shot.mjs dictation read      # just these settings panes
//   node scripts/ui-shot.mjs --target onboarding # all onboarding steps
//   node scripts/ui-shot.mjs welcome done        # named onboarding steps
//   node scripts/ui-shot.mjs --no-build          # reuse the current bundle
//   node scripts/ui-shot.mjs --out some/dir
//
// How it works (macOS, no window-automation entitlements needed):
//   1. `dev.sh --build-only` builds + signs the debug Murmur.app (the binary
//      embeds ../dist at compile time, so a rebuild is required to pick up any
//      frontend edit).
//   2. For each section it launches the binary with MURMUR_UI_SHOT + either
//      MURMUR_UI_PANE (settings) or MURMUR_UI_STEP (onboarding). A debug build
//      then opens the right window straight to that section and writes its
//      CGWindowID to MURMUR_UI_WINID_FILE (see show_main_window /
//      show_onboarding_window in lib.rs).
//   3. `screencapture -l <windowid>` grabs exactly that window.
//
// Requires Screen Recording permission for the terminal. Debug-only hooks — the
// env vars do nothing in a release build.

import { execFileSync, spawn } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, rmSync, statSync } from "node:fs";
import { join, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { tmpdir } from "node:os";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const APP = join(ROOT, "src-tauri/target/debug/Murmur.app");
const BIN = join(APP, "Contents/MacOS/murmur");

// Each target: the sections to capture and the env that deep-links one.
const TARGETS = {
  settings: {
    sections: ["home", "dictation", "read", "shortcuts", "sound", "history", "support"],
    env: (pane) => ({ MURMUR_UI_PANE: pane }),
  },
  onboarding: {
    // step key → data-step index (see onboarding.html / goTo()).
    sections: ["welcome", "permissions", "download", "try", "read", "done"],
    env: (step) => ({ MURMUR_UI_STEP: String(["welcome", "permissions", "download", "try", "read", "done"].indexOf(step)) }),
  },
};

// ---- args -----------------------------------------------------------------
const argv = process.argv.slice(2);
const flag = (name, def) => {
  const i = argv.indexOf(name);
  return i >= 0 ? argv[i + 1] : def;
};
const noBuild = argv.includes("--no-build");
const OUT = resolve(flag("--out", join(ROOT, "docs/design/shots")));
const onlyTarget = flag("--target", null);
const positional = argv.filter((a, i) => !a.startsWith("--") && (i === 0 || !argv[i - 1].startsWith("--")));

if (onlyTarget && !TARGETS[onlyTarget]) {
  console.error(`unknown target "${onlyTarget}" — one of: ${Object.keys(TARGETS).join(", ")}`);
  process.exit(2);
}

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

// ---- capture one section --------------------------------------------------
function capture(targetName, section, extraEnv) {
  kill();
  sleep(600);
  const tag = `${targetName}-${section}`;
  const widFile = join(tmpdir(), `murmur-ui-winid-${tag}`);
  rmSync(widFile, { force: true });

  const child = spawn(BIN, [], {
    detached: true,
    stdio: "ignore",
    env: { ...process.env, MURMUR_UI_SHOT: "1", MURMUR_UI_WINID_FILE: widFile, ...extraEnv },
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

  const out = join(OUT, `${tag}.png`);
  // -l <id>: capture that window · -o: omit the drop shadow · -x: no sound.
  execFileSync("screencapture", ["-l", winid, "-o", "-x", out], { stdio: "inherit" });
  kill();

  if (!existsSync(out) || statSync(out).size === 0) {
    throw new Error("screencapture produced no image (Screen Recording permission for the terminal?)");
  }
  return out;
}

// Read width/height from a PNG's IHDR (bytes 16–24) — avoids a dependency.
function pngSize(path) {
  const b = readFileSync(path);
  return { width: b.readUInt32BE(16), height: b.readUInt32BE(20) };
}

// ---- run ------------------------------------------------------------------
const targetNames = onlyTarget ? [onlyTarget] : Object.keys(TARGETS);
let failed = 0;
let ran = 0;
for (const tname of targetNames) {
  let sections = TARGETS[tname].sections;
  if (positional.length) sections = sections.filter((s) => positional.includes(s));
  if (!sections.length) continue;
  console.log(`\n${C.bold}${C.dim}━━ ${tname} → ${OUT.replace(ROOT + "/", "")}/ ━━${C.reset}`);
  for (const section of sections) {
    ran++;
    process.stdout.write(`  ${C.bold}${section}${C.reset} … `);
    try {
      const out = capture(tname, section, TARGETS[tname].env(section));
      const { width, height } = pngSize(out);
      console.log(`${C.green}✓${C.reset} ${C.dim}${width}×${height}${C.reset}`);
    } catch (e) {
      console.log(`${C.red}✗ ${e.message}${C.reset}`);
      failed++;
    }
  }
}
kill();

if (ran === 0) {
  console.error(`\n${C.red}no sections matched ${JSON.stringify(positional)}${C.reset}`);
  process.exit(2);
}
console.log(
  `\n${failed === 0 ? C.green + `✓ ${ran} shot(s) in ${OUT.replace(ROOT + "/", "")}/` : C.red + `✗ ${failed} of ${ran} failed`}${C.reset}`
);
process.exit(failed === 0 ? 0 : 1);
