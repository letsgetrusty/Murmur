#!/usr/bin/env node
// ui-diff — objective layout check against the design reference.
//
// Renders the design reference (docs/design/reference.html) and a real app
// window at identical dimensions, extracts computed box metrics (padding, size,
// radius, font, gap) for a mapped element set from BOTH, and prints a mismatch
// table. Two targets:
//   settings    — frontend/settings.html   vs the reference's #view-settings
//   onboarding  — frontend/onboarding.html vs the reference's #view-onboarding
// This replaces eyeballing two screenshots, which repeatedly missed small
// numeric drift (card padding, select width, line-height) over the redesign.
//
// Usage:
//   node scripts/ui-diff.mjs                    # every section of both targets
//   node scripts/ui-diff.mjs dictation          # one section (settings)
//   node scripts/ui-diff.mjs --target onboarding # all onboarding steps
//   node scripts/ui-diff.mjs welcome permissions # named onboarding steps
//   node scripts/ui-diff.mjs --tol 2            # loosen the px tolerance (default 1.5)
//
// Exits non-zero if any metric is outside tolerance, so it can gate a commit.
//
// It renders with the frontend JS stripped, so it measures STATIC layout only —
// JS-populated content (history rows, speed segments, download %, live badges)
// shows up as "missing" on the app side. For those, capture the live window
// with scripts/ui-shot.mjs instead.

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
const TOL = parseFloat(flag("--tol", "1.5"));
const ONLY_TARGET = flag("--target", null);
const CHROME =
  process.env.CHROME ||
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const positional = argv.filter((a, i) => !a.startsWith("--") && (i === 0 || !argv[i - 1].startsWith("--")));

// ---- spec helpers ---------------------------------------------------------
// m(key, refSel, appSel, metrics) maps one conceptual element to a selector in
// the reference and in the app (often the same class), plus the metrics that
// must match.
//
// Metrics vocabulary: width height top left right bottom (rect) ·
// paddingTop/Right/Bottom/Left marginTop/Bottom gap fontSize fontWeight
// letterSpacing borderRadius (computed style).
const m = (key, refSel, appSel, metrics) => ({ key, ref: refSel, app: appSel, metrics });
const same = (key, sel, metrics) => m(key, sel, sel, metrics);
const RECT = new Set(["width", "height", "top", "left", "right", "bottom"]);

// ===========================================================================
// TARGET: settings window (960×680)
// ===========================================================================
// Pane entries scope to `.pane.active` because hidden panes report a zero rect
// and several classes (.card/.row) repeat across panes.
const sp = (key, sel, metrics) => m(key, ".pane.active " + sel, ".pane.active " + sel, metrics);
const spM = (key, refSel, appSel, metrics) =>
  m(key, ".pane.active " + refSel, ".pane.active " + appSel, metrics);

const SETTINGS_SHELL = [
  same("sidebar", ".side", ["width"]),
  m("nav-item", ".nav button", ".nav-item", ["height", "paddingTop", "paddingLeft", "fontSize", "borderRadius"]),
  m("nav-icon", ".nav button .ic", ".nav-item .ic", ["width", "height", "borderRadius"]),
  same("topbar", ".topbar", ["height", "paddingLeft", "paddingRight"]),
  same("mic", ".mic", ["height", "paddingTop", "paddingLeft", "fontSize", "borderRadius"]),
  same("brand", ".brand", ["fontSize", "gap"]),
  same("side-foot", ".side-foot", ["height", "paddingTop"]),
];

const SETTINGS_PANES = {
  home: [
    sp("stats", ".stats", ["gap"]),
    sp("stats-cell", ".stats .st", ["paddingTop", "paddingLeft", "borderRadius"]),
    sp("stat-n", ".stats .n", ["fontSize", "fontWeight"]),
    sp("stat-k", ".stats .k", ["fontSize"]),
    sp("sec-label", ".sec-label", ["fontSize", "letterSpacing", "marginTop", "marginBottom"]),
    sp("card", ".card", ["paddingTop", "paddingBottom", "borderRadius"]),
    sp("gs-row", ".gs", ["paddingTop"]),
    sp("gs-icon", ".gs .gi", ["width", "height", "borderRadius"]),
    sp("gs-title", ".gs .gt", ["fontSize"]),
  ],
  dictation: [
    sp("sec-label", ".sec-label", ["fontSize", "letterSpacing", "marginTop", "marginBottom"]),
    sp("card", ".card", ["paddingTop", "paddingBottom", "borderRadius"]),
    sp("row", ".row", ["paddingTop"]),
    sp("lbl", ".row .lbl", ["fontSize"]),
    sp("lbl-small", ".row .lbl small", ["fontSize"]),
    // The reference model picker is a custom fly-out (.msel-btn); the app uses a
    // styled native <select>. Compare only what should visually agree.
    spM("model-ctl", ".msel-btn", ".sel", ["fontSize", "borderRadius"]),
    sp("kbd", ".kbd", ["height", "fontSize", "paddingLeft", "borderRadius"]),
    sp("ta", ".ta", ["fontSize", "borderRadius"]),
  ],
  read: [
    sp("card", ".card", ["paddingTop", "paddingBottom", "borderRadius"]),
    sp("row", ".row", ["paddingTop"]),
    sp("sel", ".sel", ["paddingTop", "paddingLeft", "fontSize", "borderRadius"]),
    // .seg is JS-populated (speed buttons) — its height collapses in the static
    // render, so measure only the CSS-driven corner. Height is covered live.
    sp("seg", ".seg", ["borderRadius"]),
    sp("toggle", ".toggle", ["width", "height", "borderRadius"]),
  ],
  shortcuts: [
    sp("sec-label", ".sec-label", ["fontSize"]),
    sp("card", ".card", ["paddingTop", "paddingBottom", "borderRadius"]),
    sp("row", ".row", ["paddingTop"]),
    // .rec recorders show their chord via JS — empty here, so skip height.
    sp("rec", ".rec", ["fontSize", "paddingLeft", "borderRadius"]),
    sp("sel", ".sel", ["paddingTop", "paddingLeft", "fontSize", "borderRadius"]),
  ],
  sound: [
    sp("sec-label", ".sec-label", ["fontSize", "marginTop", "marginBottom"]),
    sp("card", ".card", ["paddingTop", "paddingBottom", "borderRadius"]),
    sp("row", ".row", ["paddingTop"]),
    sp("toggle", ".toggle", ["width", "height", "borderRadius"]),
    sp("sel", ".sel", ["paddingTop", "paddingLeft", "fontSize", "borderRadius"]),
  ],
  history: [
    sp("search", ".search", ["height", "paddingLeft", "borderRadius"]),
    sp("sec-label", ".sec-label", ["fontSize"]),
    sp("card", ".card", ["paddingTop", "borderRadius"]),
  ],
  support: [
    sp("sec-label", ".sec-label", ["fontSize", "marginTop", "marginBottom"]),
    sp("card", ".card", ["paddingTop", "paddingBottom", "borderRadius"]),
    sp("row", ".row", ["paddingTop"]),
    sp("btn", ".btn", ["height", "paddingLeft", "borderRadius", "fontSize"]),
  ],
};

// ===========================================================================
// TARGET: onboarding modal (560×500)
// ===========================================================================
// The reference view (#view-onboarding) uses `ob-*2` class names; the shipped
// window renames them (ob-sub→ob-lead, ob-perm2→perm-row, ob-cta→.primary.block).
// Step entries scope to the one visible step: reference toggles `.active`, the
// app toggles the `hidden` attribute.
const oR = ".ob-step2.active ";
const oA = ".ob-step:not([hidden]) ";
const obTitle = m("title", oR + ".ob-title", oA + ".ob-title", ["fontSize", "fontWeight", "letterSpacing", "marginTop", "marginBottom"]);
const obLead = m("lead", oR + ".ob-sub", oA + ".ob-lead", ["fontSize", "marginTop", "marginBottom"]);

const OB_SHELL = [
  m("modal", ".ob-modal", ".ob", ["borderRadius"]),
  same("progress", ".ob-progress", ["height"]),
  m("body", ".ob-body2", ".ob-scroll", ["paddingTop", "paddingLeft", "paddingBottom"]),
  m("foot", ".ob-foot2", ".ob-foot", ["paddingTop", "paddingLeft", "paddingBottom"]),
  m("cta", ".ob-cta", ".ob-foot .primary", ["paddingTop", "borderRadius", "fontSize"]),
  m("back", ".ob-back2", ".ob-back", ["fontSize", "marginBottom"]),
];

const OB_STEPS = [
  {
    key: "welcome",
    idx: 0,
    spec: [
      ...OB_SHELL, // measured once, on the first step
      m("hero", oR + ".ob-hero", oA + ".ob-hero", ["paddingTop", "paddingBottom"]),
      obTitle,
      obLead,
    ],
  },
  {
    key: "permissions",
    idx: 1,
    spec: [
      obTitle,
      obLead,
      m("perm-row", oR + ".ob-perm2", oA + ".perm-row", ["paddingTop", "paddingLeft", "borderRadius", "marginTop"]),
      m("perm-icon", oR + ".ob-perm2 .pi", oA + ".perm-icon", ["width", "height", "borderRadius"]),
      m("perm-name", oR + ".ob-perm2 .pn", oA + ".perm-name", ["fontSize", "fontWeight"]),
      m("perm-desc", oR + ".ob-perm2 .pd", oA + ".perm-row .hint", ["fontSize"]),
    ],
  },
  {
    key: "download",
    idx: 2,
    spec: [
      obTitle,
      obLead,
      m("dl-row", oR + ".dl2", oA + ".dl-row", ["marginTop"]),
      m("dl-head", oR + ".dl2-head", oA + ".dl-head", ["fontSize", "marginBottom"]),
      m("dl-track", oR + ".dl2-track", oA + ".dl-track", ["height", "borderRadius"]),
    ],
  },
  {
    key: "try",
    idx: 3,
    spec: [
      obTitle,
      obLead,
      m("sc-pill", oR + ".sc-pill", oA + ".sc-pill", ["paddingTop", "paddingLeft", "borderRadius", "fontSize"]),
      m("ob-change", oR + ".ob-change", oA + ".ob-change", ["fontSize", "marginTop"]),
      m("ob-record", oR + ".ob-record", oA + ".ob-record", ["marginTop", "paddingTop", "paddingLeft", "borderRadius", "fontSize"]),
    ],
  },
  {
    key: "read",
    idx: 4,
    spec: [
      obTitle,
      obLead,
      m("sc-pill", oR + ".sc-pill", oA + ".sc-pill", ["paddingTop", "paddingLeft", "borderRadius", "fontSize"]),
      m("ob-change", oR + ".ob-change", oA + ".ob-change", ["fontSize", "marginTop"]),
      m("read-sample", oR + ".read-sample", oA + ".read-sample", ["fontSize", "marginTop"]),
    ],
  },
  {
    key: "done",
    idx: 5,
    spec: [
      m("check", oR + ".ob-check-lg", oA + ".ob-check", ["width", "height"]),
      obTitle,
      obLead,
    ],
  },
];

// Isolate the reference's settings window / onboarding modal: hide the other
// demos and stretch the container to fill the viewport so it lines up with the
// real app window at the same dimensions.
const REF_ISOLATE_SETTINGS = `<style>
  html,body{overflow:hidden!important;margin:0!important;}
  body>*{display:none!important;}
  #view-settings{display:block!important;}
  .win{width:100vw!important;height:100vh!important;
    margin:0!important;border:0!important;border-radius:0!important;box-shadow:none!important;}
</style>`;
const REF_ISOLATE_ONBOARDING = `<style>
  html,body{overflow:hidden!important;margin:0!important;}
  body>*{display:none!important;}
  #view-onboarding{display:block!important;}
  .ob-stage{width:100vw!important;height:100vh!important;
    margin:0!important;border:0!important;border-radius:0!important;box-shadow:none!important;}
</style>`;

const TARGETS = {
  settings: {
    app: resolve(join(ROOT, "frontend/settings.html")),
    w: 960,
    h: 680,
    isolate: REF_ISOLATE_SETTINGS,
    sections: Object.keys(SETTINGS_PANES).map((key) => ({ key })),
    spec: (sec) => [...SETTINGS_SHELL, ...SETTINGS_PANES[sec.key]],
    // Reference pane ids are bare (#home); the app prefixes them (#tab-home).
    activate: (sec, side) => {
      const id = side === "app" ? "tab-" + sec.key : sec.key;
      return `document.querySelectorAll('.pane').forEach(function(p){p.classList.toggle('active',p.id===${JSON.stringify(id)});});`;
    },
  },
  onboarding: {
    app: resolve(join(ROOT, "frontend/onboarding.html")),
    w: 560,
    h: 500,
    isolate: REF_ISOLATE_ONBOARDING,
    sections: OB_STEPS.map((s) => ({ key: s.key, idx: s.idx })),
    spec: (sec) => OB_STEPS.find((s) => s.key === sec.key).spec,
    activate: (sec, side) =>
      side === "ref"
        ? `document.querySelectorAll('.ob-step2').forEach(function(s,i){s.classList.toggle('active',i===${sec.idx});});`
        : `document.querySelectorAll('.ob-step').forEach(function(s,i){if(i===${sec.idx})s.removeAttribute('hidden');else s.setAttribute('hidden','');});`,
  },
};

// ---- in-page extractor ----------------------------------------------------
// Runs synchronously at end of body (after CSS is applied and the DOM is fully
// parsed), runs the target's activation JS to show the desired section — so it
// never depends on the page's own JS — measures each selector, and stashes
// base64 JSON in a <pre> that --dump-dom then echoes back to us.
function extractorScript(specs, activateJs) {
  const SPECS = JSON.stringify(specs.map((s) => ({ key: s.key, sel: s.sel, metrics: s.metrics })));
  const RECTS = JSON.stringify([...RECT]);
  return `<script>(function(){
  try {
    ${activateJs}
    var SPECS=${SPECS}, RECT=${RECTS};
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

// ---- render ---------------------------------------------------------------
const tmp = mkdtempSync(join(tmpdir(), "ui-diff-"));

function renderMetrics(kind, htmlPath, specs, activateJs, isolate, W, H) {
  let html = readFileSync(htmlPath, "utf8");
  const baseDir = dirname(htmlPath);

  if (kind === "app") {
    // Resolve the relative stylesheet links against the real frontend dir and
    // drop the ES-module script (needs a server; we measure static layout).
    html = html.replace(/href="\.\/([^"]+)"/g, (_, f) => `href="${pathToFileURL(join(baseDir, f)).href}"`);
    html = html.replace(/<script[^>]*type="module"[^>]*><\/script>/g, "");
  }

  const selSpecs = specs.map((s) => ({ key: s.key, sel: kind === "app" ? s.app : s.ref, metrics: s.metrics }));
  const inject = (kind === "ref" ? isolate : "") + extractorScript(selSpecs, activateJs);
  // Append at the very end so the extractor runs after the whole body is parsed
  // and styled. Works whether or not a </body> tag is present.
  html = html.includes("</body>") ? html.replace("</body>", inject + "</body>") : html + inject;

  const file = join(tmp, `${kind}-${Math.abs(hash(activateJs))}.html`);
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
  if (!mtch) throw new Error(`${kind}: no metrics returned (page failed to render?)`);
  const json = Buffer.from(mtch[1], "base64").toString("utf8");
  const data = JSON.parse(json);
  if (data.__error) throw new Error(`${kind}: extractor error: ${data.__error}`);
  return data;
}

const hash = (s) => {
  let h = 0;
  for (let i = 0; i < s.length; i++) h = (Math.imul(31, h) + s.charCodeAt(i)) | 0;
  return h;
};

// ---- compare + report -----------------------------------------------------
const C = { red: "\x1b[31m", green: "\x1b[32m", yellow: "\x1b[33m", dim: "\x1b[2m", bold: "\x1b[1m", reset: "\x1b[0m" };
const fmt = (v) => (v === undefined || v === null ? "—" : typeof v === "number" ? String(v) : v);

function diffSection(target, sec) {
  const specs = target.spec(sec);
  const refM = renderMetrics("ref", REF, specs, target.activate(sec, "ref"), target.isolate, target.w, target.h);
  const appM = renderMetrics("app", target.app, specs, target.activate(sec, "app"), target.isolate, target.w, target.h);

  const rows = [];
  for (const s of specs) {
    const r = refM[s.key];
    const a = appM[s.key];
    if (r === null || a === null) {
      rows.push({
        el: s.key,
        metric: r === null && a === null ? "(absent both)" : r === null ? "(ref missing)" : "(app missing)",
        ref: "",
        app: "",
        delta: "",
        bad: r === null && a === null ? false : "miss",
      });
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

function printSection(label, rows) {
  const bad = rows.filter((r) => r.bad);
  const head = bad.length === 0 ? `${C.green}✓${C.reset}` : `${C.red}✗ ${bad.length}${C.reset}`;
  console.log(`\n${C.bold}${label}${C.reset}  ${head}`);
  const w = { el: 12, metric: 14, ref: 8, app: 8 };
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
const targetNames = ONLY_TARGET ? [ONLY_TARGET] : Object.keys(TARGETS);
if (ONLY_TARGET && !TARGETS[ONLY_TARGET]) {
  console.error(`unknown target "${ONLY_TARGET}" — one of: ${Object.keys(TARGETS).join(", ")}`);
  process.exit(2);
}

console.log(`${C.dim}ui-diff · tol ±${TOL}px · ref=${REF.replace(ROOT + "/", "")}${C.reset}`);
console.log(`${C.dim}(set UI_DIFF_ALL=1 to print every metric, not just mismatches)${C.reset}`);

let totalBad = 0;
let ran = 0;
for (const tname of targetNames) {
  const target = TARGETS[tname];
  let sections = target.sections;
  if (positional.length) sections = sections.filter((s) => positional.includes(s.key));
  if (!sections.length) continue;
  console.log(`\n${C.bold}${C.dim}━━ ${tname} · ${target.w}×${target.h} ━━${C.reset}`);
  for (const sec of sections) {
    ran++;
    try {
      const rows = diffSection(target, sec);
      totalBad += rows.filter((r) => r.bad === true).length;
      printSection(sec.key, rows);
    } catch (e) {
      console.error(`\n${C.red}${sec.key}: ${e.message}${C.reset}`);
      totalBad++;
    }
  }
}

if (ran === 0) {
  console.error(`\n${C.red}no sections matched ${JSON.stringify(positional)}${C.reset}`);
  process.exit(2);
}
console.log(
  `\n${totalBad === 0 ? C.green + "✓ all metrics within tolerance" : C.red + `✗ ${totalBad} metric mismatch(es)`}${C.reset}`
);
process.exit(totalBad === 0 ? 0 : 1);
