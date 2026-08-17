#!/usr/bin/env node
// ui-diff — objective layout check against the design reference.
//
// Renders the design reference (docs/design/reference.html) and the real app
// window (frontend/settings.html) at identical dimensions, then extracts
// computed box metrics (padding, size, radius, font, gap) for a mapped set of
// elements from BOTH and prints a mismatch table. This replaces eyeballing two
// screenshots — almost every bug in the settings redesign was a number that was
// off by a few px (card padding, select width, nav accent), which this catches
// mechanically.
//
// Usage:
//   node scripts/ui-diff.mjs                 # all panes
//   node scripts/ui-diff.mjs dictation       # one pane
//   node scripts/ui-diff.mjs --tol 2         # loosen the px tolerance (default 1.5)
//   node scripts/ui-diff.mjs --ref other.html --app path/to/settings.html
//
// Exits non-zero if any metric is outside tolerance, so it can gate a commit.
//
// It renders with the frontend JS stripped, so it measures STATIC layout only —
// JS-populated content (history rows, the speed segments, voice list) shows up
// as "missing" on the app side. For those, capture the live window instead.

import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");

// ---- args -----------------------------------------------------------------
const argv = process.argv.slice(2);
const flag = (name, def) => {
  const i = argv.indexOf(name);
  return i >= 0 ? argv[i + 1] : def;
};
const REF = resolve(flag("--ref", join(ROOT, "docs/design/reference.html")));
const APP = resolve(flag("--app", join(ROOT, "frontend/settings.html")));
const TOL = parseFloat(flag("--tol", "1.5"));
const W = parseInt(flag("--width", "960"), 10);
const H = parseInt(flag("--height", "680"), 10);
const CHROME =
  process.env.CHROME ||
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const positional = argv.filter((a, i) => !a.startsWith("--") && (i === 0 || !argv[i - 1].startsWith("--")));
const ONLY_PANE = positional[0] || null;

// ---- comparison spec ------------------------------------------------------
// Each entry maps one conceptual element to a selector in the reference and in
// the app (usually the same class), plus the metrics that must match. Pane
// entries are scoped to `.pane.active` because hidden panes report a zero rect
// and several classes (.card/.row) repeat across panes.
//
//   m(key, refSel, appSel, [metrics])
const m = (key, refSel, appSel, metrics) => ({ key, ref: refSel, app: appSel, metrics });
const same = (key, sel, metrics) => m(key, sel, sel, metrics);
const pane = (key, sel, metrics) => m(key, ".pane.active " + sel, ".pane.active " + sel, metrics);
// Pane element whose reference and app selectors differ (a control the app
// implements with a different element than the mockup).
const paneM = (key, refSel, appSel, metrics) =>
  m(key, ".pane.active " + refSel, ".pane.active " + appSel, metrics);

// Metrics vocabulary: width height top left right bottom (rect) ·
// paddingTop/Right/Bottom/Left marginTop/Bottom gap fontSize fontWeight
// letterSpacing borderRadius (computed style).
const SHELL = [
  same("sidebar", ".side", ["width"]),
  m("nav-item", ".nav button", ".nav-item", ["height", "paddingTop", "paddingLeft", "fontSize", "borderRadius"]),
  m("nav-icon", ".nav button .ic", ".nav-item .ic", ["width", "height", "borderRadius"]),
  same("topbar", ".topbar", ["height", "paddingLeft", "paddingRight"]),
  same("mic", ".mic", ["height", "paddingTop", "paddingLeft", "fontSize", "borderRadius"]),
  same("brand", ".brand", ["fontSize", "gap"]),
  same("side-foot", ".side-foot", ["height", "paddingTop"]),
];

const PANES = {
  home: [
    pane("stats", ".stats", ["gap"]),
    pane("stats-cell", ".stats .st", ["paddingTop", "paddingLeft", "borderRadius"]),
    pane("stat-n", ".stats .n", ["fontSize", "fontWeight"]),
    pane("stat-k", ".stats .k", ["fontSize"]),
    pane("sec-label", ".sec-label", ["fontSize", "letterSpacing", "marginTop", "marginBottom"]),
    pane("card", ".card", ["paddingTop", "paddingBottom", "borderRadius"]),
    pane("gs-row", ".gs", ["paddingTop"]),
    pane("gs-icon", ".gs .gi", ["width", "height", "borderRadius"]),
    pane("gs-title", ".gs .gt", ["fontSize"]),
  ],
  dictation: [
    pane("sec-label", ".sec-label", ["fontSize", "letterSpacing", "marginTop", "marginBottom"]),
    pane("card", ".card", ["paddingTop", "paddingBottom", "borderRadius"]),
    pane("row", ".row", ["paddingTop"]),
    pane("lbl", ".row .lbl", ["fontSize"]),
    pane("lbl-small", ".row .lbl small", ["fontSize"]),
    // The reference model picker is a custom fly-out (.msel-btn); the app uses a
    // styled native <select>. Compare only what should visually agree.
    paneM("model-ctl", ".msel-btn", ".sel", ["fontSize", "borderRadius"]),
    pane("kbd", ".kbd", ["height", "fontSize", "paddingLeft", "borderRadius"]),
    pane("ta", ".ta", ["fontSize", "borderRadius"]),
  ],
  read: [
    pane("card", ".card", ["paddingTop", "paddingBottom", "borderRadius"]),
    pane("row", ".row", ["paddingTop"]),
    pane("sel", ".sel", ["paddingTop", "paddingLeft", "fontSize", "borderRadius"]),
    // .seg is JS-populated (speed buttons) — its height collapses in the static
    // render, so measure only the CSS-driven corner. Height is covered live.
    pane("seg", ".seg", ["borderRadius"]),
    pane("toggle", ".toggle", ["width", "height", "borderRadius"]),
  ],
  shortcuts: [
    pane("sec-label", ".sec-label", ["fontSize"]),
    pane("card", ".card", ["paddingTop", "paddingBottom", "borderRadius"]),
    pane("row", ".row", ["paddingTop"]),
    // .rec recorders show their chord via JS — empty here, so skip height.
    pane("rec", ".rec", ["fontSize", "paddingLeft", "borderRadius"]),
    pane("sel", ".sel", ["paddingTop", "paddingLeft", "fontSize", "borderRadius"]),
  ],
  sound: [
    pane("sec-label", ".sec-label", ["fontSize", "marginTop", "marginBottom"]),
    pane("card", ".card", ["paddingTop", "paddingBottom", "borderRadius"]),
    pane("row", ".row", ["paddingTop"]),
    pane("toggle", ".toggle", ["width", "height", "borderRadius"]),
    pane("sel", ".sel", ["paddingTop", "paddingLeft", "fontSize", "borderRadius"]),
  ],
  history: [
    pane("search", ".search", ["height", "paddingLeft", "borderRadius"]),
    pane("sec-label", ".sec-label", ["fontSize"]),
    pane("card", ".card", ["paddingTop", "borderRadius"]),
  ],
  support: [
    pane("sec-label", ".sec-label", ["fontSize", "marginTop", "marginBottom"]),
    pane("card", ".card", ["paddingTop", "paddingBottom", "borderRadius"]),
    pane("row", ".row", ["paddingTop"]),
    pane("btn", ".btn", ["height", "paddingLeft", "borderRadius", "fontSize"]),
  ],
};

// Reference pane ids are bare (#home); the app prefixes them (#tab-home).
const REF_PANE = (p) => p;
const APP_PANE = (p) => "tab-" + p;

const RECT = new Set(["width", "height", "top", "left", "right", "bottom"]);

// ---- in-page extractor ----------------------------------------------------
// Runs synchronously at end of body (after CSS is applied and the DOM is fully
// parsed), activates the target pane itself — so it never depends on the page's
// own JS — measures each selector, and stashes base64 JSON in a <pre> that
// --dump-dom then echoes back to us.
function extractorScript(specs, paneId) {
  const SPECS = JSON.stringify(specs.map((s) => ({ key: s.key, sel: s.sel, metrics: s.metrics })));
  const RECTS = JSON.stringify([...RECT]);
  return `<script>(function(){
  try {
    var PANE=${JSON.stringify(paneId)}, SPECS=${SPECS}, RECT=${RECTS};
    var panes=document.querySelectorAll('.pane');
    for(var i=0;i<panes.length;i++){panes[i].classList.toggle('active',panes[i].id===PANE);}
    function num(v){var n=parseFloat(v);return isFinite(n)?Math.round(n*10)/10:String(v);}
    var out={};
    for(var j=0;j<SPECS.length;j++){
      var s=SPECS[j], el=document.querySelector(s.sel);
      if(!el){out[s.key]=null;continue;}
      var r=el.getBoundingClientRect(), cs=getComputedStyle(el), o={};
      for(var k=0;k<s.metrics.length;k++){
        var mm=s.metrics[k];
        if(RECT.indexOf(mm)>=0) o[mm]=num(r[mm]);
        else if(mm==='borderRadius') o[mm]=num(cs.borderTopLeftRadius);
        else o[mm]=num(cs[mm]);
      }
      out[s.key]=o;
    }
    var pre=document.createElement('pre');pre.id='__UIDIFF__';pre.style.display='none';
    pre.textContent=btoa(unescape(encodeURIComponent(JSON.stringify(out))));
    document.body.appendChild(pre);
  }catch(e){
    var p=document.createElement('pre');p.id='__UIDIFF__';
    p.textContent=btoa('{"__error":'+JSON.stringify(String(e))+'}');
    document.body.appendChild(p);
  }
})();</script>`;
}

// Isolate the reference's settings window: hide the other demos (overlay pill,
// onboarding) and stretch .win to fill the viewport, so it lines up with the
// app window at the same dimensions.
const REF_ISOLATE = `<style>
  html,body{overflow:hidden!important;margin:0!important;}
  body>*{display:none!important;}
  #view-settings{display:block!important;}
  .win{width:100vw!important;height:100vh!important;
    margin:0!important;border:0!important;border-radius:0!important;box-shadow:none!important;}
</style>`;

// ---- render ---------------------------------------------------------------
const tmp = mkdtempSync(join(tmpdir(), "ui-diff-"));

function renderMetrics(kind, htmlPath, specs, paneId) {
  let html = readFileSync(htmlPath, "utf8");
  const baseDir = dirname(htmlPath);

  if (kind === "app") {
    // Resolve the relative stylesheet links against the real frontend dir and
    // drop the ES-module script (needs a server; we measure static layout).
    html = html.replace(/href="\.\/([^"]+)"/g, (_, f) => `href="${pathToFileURL(join(baseDir, f)).href}"`);
    html = html.replace(/<script[^>]*type="module"[^>]*><\/script>/g, "");
  }

  const selSpecs = specs.map((s) => ({ key: s.key, sel: kind === "app" ? s.app : s.ref, metrics: s.metrics }));
  const inject = (kind === "ref" ? REF_ISOLATE : "") + extractorScript(selSpecs, paneId);
  // Append at the very end so the extractor runs after the whole body is parsed
  // and styled. Works whether or not a </body> tag is present.
  html = html.includes("</body>") ? html.replace("</body>", inject + "</body>") : html + inject;

  const file = join(tmp, `${kind}-${paneId}.html`);
  writeFileSync(file, html);

  const out = execFileSync(
    CHROME,
    [
      "--headless=new",
      "--disable-gpu",
      "--hide-scrollbars",
      "--force-device-scale-factor=1",
      `--window-size=${W},${H}`,
      "--allow-file-access-from-files",
      "--dump-dom",
      "--virtual-time-budget=1500",
      pathToFileURL(file).href,
    ],
    { encoding: "utf8", maxBuffer: 64 * 1024 * 1024, stdio: ["ignore", "pipe", "ignore"] }
  );

  const mtch = out.match(/<pre id="__UIDIFF__"[^>]*>([A-Za-z0-9+/=]*)<\/pre>/);
  if (!mtch) throw new Error(`${kind}/${paneId}: no metrics returned (page failed to render?)`);
  const json = Buffer.from(mtch[1], "base64").toString("utf8");
  const data = JSON.parse(json);
  if (data.__error) throw new Error(`${kind}/${paneId}: extractor error: ${data.__error}`);
  return data;
}

// ---- compare + report -----------------------------------------------------
const C = { red: "\x1b[31m", green: "\x1b[32m", yellow: "\x1b[33m", dim: "\x1b[2m", bold: "\x1b[1m", reset: "\x1b[0m" };
const fmt = (v) => (v === undefined || v === null ? "—" : typeof v === "number" ? String(v) : v);

function diffPane(p) {
  const specs = [...SHELL, ...(PANES[p] || [])];
  const refM = renderMetrics("ref", REF, specs, REF_PANE(p));
  const appM = renderMetrics("app", APP, specs, APP_PANE(p));

  const rows = [];
  for (const s of specs) {
    const r = refM[s.key];
    const a = appM[s.key];
    if (r === null || a === null) {
      rows.push({ el: s.key, metric: r === null && a === null ? "(absent both)" : r === null ? "(ref missing)" : "(app missing)", ref: "", app: "", delta: "", bad: r === null && a === null ? false : "miss" });
      continue;
    }
    for (const metric of s.metrics) {
      const rv = r[metric];
      const av = a[metric];
      let bad = false;
      let delta = "";
      if (typeof rv === "number" && typeof av === "number") {
        const d = Math.round((av - rv) * 10) / 10;
        delta = d > 0 ? `+${d}` : String(d);
        bad = Math.abs(d) > TOL;
      } else {
        bad = String(rv) !== String(av);
        delta = bad ? "≠" : "=";
      }
      rows.push({ el: s.key, metric, ref: fmt(rv), app: fmt(av), delta, bad });
    }
  }
  return rows;
}

function printPane(p, rows) {
  const bad = rows.filter((r) => r.bad);
  const head = bad.length === 0 ? `${C.green}✓${C.reset}` : `${C.red}✗ ${bad.length}${C.reset}`;
  console.log(`\n${C.bold}${p}${C.reset}  ${head}`);
  const w = { el: 12, metric: 14, ref: 8, app: 8, delta: 7 };
  for (const r of rows) w.el = Math.max(w.el, r.el.length);
  const pad = (s, n) => String(s).padEnd(n);
  const shown = process.env.UI_DIFF_ALL ? rows : rows.filter((r) => r.bad || r.metric.startsWith("("));
  if (shown.length === 0) return;
  console.log(C.dim + "  " + pad("element", w.el) + pad("metric", w.metric) + pad("ref", w.ref) + pad("app", w.app) + "delta" + C.reset);
  for (const r of shown) {
    const mark = r.bad === "miss" ? C.yellow : r.bad ? C.red : C.dim;
    console.log("  " + mark + pad(r.el, w.el) + pad(r.metric, w.metric) + pad(r.ref, w.ref) + pad(r.app, w.app) + r.delta + C.reset);
  }
}

// ---- main -----------------------------------------------------------------
const panes = ONLY_PANE ? [ONLY_PANE] : Object.keys(PANES);
if (ONLY_PANE && !PANES[ONLY_PANE]) {
  console.error(`unknown pane "${ONLY_PANE}" — one of: ${Object.keys(PANES).join(", ")}`);
  process.exit(2);
}

console.log(`${C.dim}ui-diff · ${W}×${H} · tol ±${TOL}px · ref=${REF.replace(ROOT + "/", "")}${C.reset}`);
console.log(`${C.dim}(set UI_DIFF_ALL=1 to print every metric, not just mismatches)${C.reset}`);

let totalBad = 0;
for (const p of panes) {
  try {
    const rows = diffPane(p);
    totalBad += rows.filter((r) => r.bad === true).length;
    printPane(p, rows);
  } catch (e) {
    console.error(`\n${C.red}${p}: ${e.message}${C.reset}`);
    totalBad++;
  }
}

console.log(
  `\n${totalBad === 0 ? C.green + "✓ all metrics within tolerance" : C.red + `✗ ${totalBad} metric mismatch(es)`}${C.reset}`
);
process.exit(totalBad === 0 ? 0 : 1);
