// Export the system style sheet as a standalone HTML page that shows the widgets
// the way the Widget Gallery does, to experiment with a style outside idealos.
//
//   node tools/style-export.ts [EDITS.css] [-o OUT.html]
//   make style-export [EDITS=my.css] [OUT=style.html]
//
// The page holds the built-in sheet (crates/compositor/src/system.css) followed by
// EDITS if given (`osctl style get system` prints the device's). Both are in
// editable boxes on the page: the preview restyles as you type, translating the
// idealos dialect to browser CSS (kind selectors become classes, `:pressed` is
// `:active`, `@dark` is the Dark switch). Copy your edits back with
// `osctl style set system FILE` or `make style-watch`.
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const REPO = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const args = process.argv.slice(2);
let out = "style.html";
let edits = "";
for (let i = 0; i < args.length; i++) {
  if (args[i] === "-o") out = args[++i] ?? out;
  else if (args[i]) edits = readFileSync(args[i], "utf8");
}
const builtin = readFileSync(join(REPO, "crates/compositor/src/system.css"), "utf8");

const esc = (s: string) => s.replace(/&/g, "&amp;").replace(/</g, "&lt;");

const page = `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>idealos style</title>
<style>
/* Structure only: sizes and shapes as the compositor draws them. Colours and
   fonts all come from the sheet below. */
* { box-sizing: border-box; }
body { margin: 0; background: var(--desktop); font: inherit; color: var(--text); font-family: var(--w-font); font-size: var(--w-size); }
.page { display: grid; grid-template-columns: minmax(0, 1fr) 420px; gap: 20px; padding: 20px; min-height: 100vh; }
@media (max-width: 900px) { .page { grid-template-columns: 1fr; } }
.w-window { border: 1px solid var(--border); border-radius: 10px; overflow: hidden; background: var(--surface); box-shadow: 0 8px 30px #0003; }
.titlebar { height: 38px; display: flex; align-items: center; padding: 0 14px; gap: 8px; cursor: default; user-select: none; }
.titlebar-line { height: 1px; }
.window-title { font-weight: bold; font-size: 13px; flex: 1; }
.content { padding: 16px; display: flex; flex-direction: column; gap: 14px; }
h3 { margin: 6px 0 0; font-size: 13px; color: var(--muted); font-weight: 600; }
.row { display: flex; gap: 8px; align-items: center; flex-wrap: wrap; }
.w-button, .w-select, .w-menu { height: 32px; padding: 0 14px; border: 1px solid; border-radius: 6px; font: inherit; color: inherit; display: inline-flex; align-items: center; gap: 8px; cursor: default; }
.w-select, .w-menu { min-width: 140px; justify-content: space-between; }
.w-select::after, .w-menu::after { content: "▾"; color: var(--muted); font-size: 11px; }
.w-checkbox, .w-toggle { display: inline-flex; align-items: center; gap: 8px; cursor: default; user-select: none; background: none !important; }
.w-checkbox .box { width: 18px; height: 18px; border: 1px solid var(--w-border, var(--control-border)); border-radius: 4px; background: var(--w-bg); display: grid; place-items: center; color: var(--accent-text); font-size: 12px; }
.w-checkbox.is-checked .box { background: var(--w-accent); border-color: var(--w-accent); }
.w-checkbox.is-checked .box::after { content: "✓"; }
.w-toggle .track { width: 34px; height: 20px; border-radius: 10px; background: var(--w-bg); position: relative; transition: background .12s; }
.w-toggle .track::after { content: ""; position: absolute; top: 2px; left: 2px; width: 16px; height: 16px; border-radius: 50%; background: #fff; transition: left .12s; }
.w-toggle.is-checked .track { background: var(--w-accent); }
.w-toggle.is-checked .track::after { left: 16px; }
.w-slider { width: 200px; height: 24px; position: relative; background: none !important; }
.w-slider .rail { position: absolute; left: 0; right: 0; top: 10px; height: 4px; border-radius: 2px; background: var(--w-bg); }
.w-slider .fill { position: absolute; left: 0; width: 60%; top: 10px; height: 4px; border-radius: 2px; background: var(--w-accent); }
.w-slider .knob { position: absolute; left: calc(60% - 9px); top: 3px; width: 18px; height: 18px; border-radius: 50%; background: #fff; border: 1px solid var(--control-border); }
.w-text_field, .w-text_area { border: 1px solid; border-radius: 6px; padding: 6px 10px; font: inherit; color: inherit; outline: none; width: 100%; }
.w-text_field:focus, .w-text_area:focus { border-width: 2px; padding: 5px 9px; }
.w-text_area { height: 90px; resize: vertical; }
.w-text_field::placeholder, .w-text_area::placeholder { color: var(--w-placeholder, var(--muted)); }
.w-text_field::selection, .w-text_area::selection { background: var(--w-selection, var(--selection)); }
.w-toolbar { display: flex; gap: 8px; padding: 8px 12px; margin: -16px -16px 0; }
.w-split { display: grid; grid-template-columns: 1fr 6px 1fr; height: 80px; border: 1px solid var(--border); border-radius: 6px; overflow: hidden; }
.w-split .divider { background: var(--w-border); cursor: ew-resize; }
.w-split > div:not(.divider) { padding: 8px; color: var(--muted); }
.w-split .divider:hover { background: var(--w-border-hover, var(--w-border)); }
.w-dialog { border: 1px solid; border-radius: 10px; padding: 16px; display: flex; flex-direction: column; gap: 12px; }
.w-terminal { border-radius: 4px; padding: 8px; font-size: 13px; white-space: pre; }
.w-terminal .cur { background: var(--w-caret); color: var(--terminal); }
.w-image { height: 64px; border: 1px solid; border-radius: 4px; display: grid; place-items: center; color: var(--muted); }
.scroll { height: 90px; overflow-y: scroll; border: 1px solid var(--border); border-radius: 6px; padding: 8px; scrollbar-color: var(--scrollbar) transparent; }
.side { display: flex; flex-direction: column; gap: 10px; }
.side textarea { width: 100%; font: 12px/1.45 ui-monospace, monospace; padding: 10px; border-radius: 8px; border: 1px solid var(--border); background: var(--surface); color: var(--text); resize: vertical; }
#builtin { height: 260px; } #edits { height: 200px; }
.status { font-size: 12px; color: var(--muted); }
.bar { display: flex; gap: 12px; align-items: center; flex-wrap: wrap; }
</style>
<style id="theme"></style>
</head>
<body>
<div class="page">
  <div class="w-window">
    <div class="titlebar is-focus"><span class="window-title is-focus">Widget Gallery</span><span class="window-title">— active</span></div>
    <div class="titlebar-line"></div>
    <div class="content">
      <div class="w-toolbar"><button class="w-button">Open dialog</button><span class="status">0 clicks</span></div>
      <h3>Buttons</h3>
      <div class="row"><button class="w-button">Click me</button><button class="w-button primary">Accent</button><button class="w-button" disabled>Disabled</button></div>
      <h3>Checkbox and toggle</h3>
      <div class="row">
        <span class="w-checkbox is-checked" tabindex="0"><span class="box"></span>Wi-Fi</span>
        <span class="w-checkbox" tabindex="0"><span class="box"></span>Bluetooth</span>
        <span class="w-toggle is-checked" tabindex="0"><span class="track"></span>Dark mode</span>
        <span class="w-toggle" tabindex="0"><span class="track"></span>Airplane</span>
      </div>
      <h3>Slider</h3>
      <div class="w-slider"><div class="rail"></div><div class="fill"></div><div class="knob"></div></div>
      <h3>Select and menu</h3>
      <div class="row"><span class="w-select" tabindex="0">Apple</span><span class="w-menu" tabindex="0">Edit</span></div>
      <h3>Text</h3>
      <input class="w-text_field" placeholder="A text field">
      <textarea class="w-text_area">A text area. Select some of this to see the selection colour.</textarea>
      <h3>Split view</h3>
      <div class="w-split"><div>Left</div><div class="divider"></div><div>Right</div></div>
      <h3>Scroll</h3>
      <div class="scroll">${"Scrolled content line.<br>".repeat(12)}</div>
      <h3>Dialog</h3>
      <div class="w-dialog"><strong>Delete note?</strong><span class="status">This can be undone from history.</span><div class="row"><button class="w-button">Cancel</button><button class="w-button primary">Delete</button></div></div>
      <h3>Image and terminal</h3>
      <div class="w-image">image</div>
      <div class="w-terminal">$ osctl style get system
/* edits */ <span class="cur"> </span></div>
    </div>
  </div>
  <div class="side">
    <div class="bar"><label><input type="checkbox" id="dark"> Dark (@dark)</label><span class="status" id="status"></span></div>
    <div class="status">Your edits (come after the built-in sheet, as on the device):</div>
    <textarea id="edits" spellcheck="false">${esc(edits)}</textarea>
    <div class="status">Built-in system sheet:</div>
    <textarea id="builtin" spellcheck="false">${esc(builtin)}</textarea>
  </div>
</div>
<script>
// Translate the idealos style dialect to browser CSS. Kinds become .w-<kind>
// classes, states become pseudo-classes or is-<state> classes (for widgets drawn
// as plain elements), and a few properties become variables the structure reads.
const STATE = { hover: ":is(:hover,.is-hover)", pressed: ":is(:active,.is-pressed)", focus: ":is(:focus,.is-focus)", disabled: ":is(:disabled,.is-disabled)", checked: ".is-checked" };
const PROPS = { "selection-color": "--w-selection", "placeholder-color": "--w-placeholder", "accent-color": "--w-accent" };
function selector(sel, dark) {
  return sel.split(",").map((s) => {
    s = s.trim();
    if (s === ":root") return dark ? "html.dark" : ":root";
    const m = s.match(/^([a-z_]+)?(\\.[a-z0-9-]+)?(?::([a-z]+))?$/i);
    if (!m) throw new Error("can't read selector " + s);
    const out = (m[1] ? ".w-" + m[1] : "") + (m[2] || "") + (m[3] ? STATE[m[3]] || ":" + m[3] : "");
    return (dark ? "html.dark " : "") + out;
  }).join(", ");
}
function declarations(body) {
  return body.split(";").map((d) => d.trim()).filter(Boolean).map((d) => {
    const i = d.indexOf(":"); const k = d.slice(0, i).trim(); const v = d.slice(i + 1).trim();
    const extra = [];
    if (k === "background") extra.push("--w-bg: " + v);
    if (k === "border-color") extra.push("--w-border: " + v);
    if (k === "caret-color") extra.push("--w-caret: " + v);
    if (k === "accent-color") extra.push("accent-color: " + v);
    if (k === "font-family") extra.push("--w-font: " + v);
    if (k === "font-size") extra.push("--w-size: " + v);
    return [(PROPS[k] || k) + ": " + v, ...extra].join("; ");
  }).join("; ");
}
function translate(css) {
  css = css.replace(/\\/\\*[\\s\\S]*?\\*\\//g, "");
  let out = "", i = 0;
  const rules = (text, dark) => {
    const re = /([^{}]+)\\{([^{}]*)\\}/g; let m, r = "";
    while ((m = re.exec(text))) r += selector(m[1], dark) + " { " + declarations(m[2]) + " }\\n";
    return r;
  };
  while (i < css.length) {
    const at = css.indexOf("@dark", i);
    if (at < 0) { out += rules(css.slice(i), false); break; }
    out += rules(css.slice(i, at), false);
    let j = css.indexOf("{", at) + 1, depth = 1;
    const start = j;
    while (depth && j < css.length) { if (css[j] === "{") depth++; else if (css[j] === "}") depth--; j++; }
    out += rules(css.slice(start, j - 1), true);
    i = j;
  }
  return out;
}
// The structure reads the root's accent and fonts; a rule may override per widget.
const ROOT = ":root { --w-accent: var(--accent); --w-font: system-ui; --w-size: 14px; }\\n";
const $ = (id) => document.getElementById(id);
function apply() {
  try {
    $("theme").textContent = ROOT + translate($("builtin").value + "\\n" + $("edits").value);
    $("status").textContent = "Applied";
  } catch (e) { $("status").textContent = "Not applied: " + e.message; }
}
for (const id of ["builtin", "edits"]) $(id).addEventListener("input", apply);
$("dark").addEventListener("change", (e) => document.documentElement.classList.toggle("dark", e.target.checked));
for (const el of document.querySelectorAll(".w-checkbox, .w-toggle")) el.addEventListener("click", () => el.classList.toggle("is-checked"));
apply();
</script>
</body>
</html>
`;
writeFileSync(out, page);
console.log(`wrote ${out}${edits ? " (with your edits)" : ""}`);
