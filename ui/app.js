const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = id => document.getElementById(id);
const el = (tag, cls, text) => {
  const node = document.createElement(tag);
  if (cls) node.className = cls;
  if (text != null) node.textContent = text;
  return node;
};

let last = null;
let lastFit = "";
let announcedAt = null;
const levels = new Map(); // "provider/index/label" -> fraction left shown last time

// "resets_at" is epoch seconds or an ISO string; say how long from now.
function until(resetsAt) {
  if (resetsAt == null) return "";
  const at = typeof resetsAt === "number" ? resetsAt * 1000 : Date.parse(resetsAt);
  const mins = Math.round((at - Date.now()) / 60000);
  if (!Number.isFinite(mins)) return "";
  if (mins <= 0) return "resets now";
  if (mins < 60) return `in ${mins}m`;
  if (mins < 48 * 60) return `in ${Math.floor(mins / 60)}h ${mins % 60}m`;
  return `in ${Math.round(mins / 1440)}d`;
}

// A drawn warning sign, so a failed provider is marked by shape as well as colour.
function alertIcon() {
  const ns = "http://www.w3.org/2000/svg";
  const svg = document.createElementNS(ns, "svg");
  svg.setAttribute("viewBox", "0 0 18 18");
  svg.setAttribute("aria-hidden", "true");
  const path = document.createElementNS(ns, "path");
  path.setAttribute("d", "M9 2.5 16.5 15.5h-15L9 2.5ZM9 7.5v3.75M9 13.4v.1");
  for (const [k, v] of [["fill", "none"], ["stroke", "currentColor"], ["stroke-width", "1.6"],
    ["stroke-linecap", "round"], ["stroke-linejoin", "round"]]) path.setAttribute(k, v);
  svg.append(path);
  return svg;
}

function quotaBar(provider, q, i, snap, settle) {
  const left = Math.max(0, Math.min(1, 1 - q.used));
  const pct = Math.round(left * 100);
  const reset = until(q.resets_at);
  const state = left < snap.low_below ? " low" : left < snap.warn_below ? " warn" : "";
  const said = `${q.label}: ${pct}% left` + (reset ? `, resets ${reset}` : "");

  const box = el("div", "quota" + state);
  box.setAttribute("role", "meter");
  box.setAttribute("aria-valuemin", 0);
  box.setAttribute("aria-valuemax", 100);
  box.setAttribute("aria-valuenow", pct);
  box.setAttribute("aria-label", said);
  box.title = `${provider.name} ${said}`;

  // A known quota always shows at least a sliver.
  const level = Math.max(left, 0.02);
  // The position too: a custom command may repeat (or leave out) labels.
  const key = provider.id + "/" + i + "/" + q.label;
  const fill = el("div", "fill");
  fill.style.setProperty("--level", levels.get(key) ?? level);
  settle.push(() => fill.style.setProperty("--level", level));
  levels.set(key, level);

  const track = el("div", "track");
  track.append(fill);
  box.append(el("div", "pct", pct + "%"), track, el("div", "label", q.label), el("div", "reset", reset));
  return box;
}

function render(snap) {
  last = snap;
  const groups = $("groups");
  const active = snap.providers.filter(p => p.configured);
  const missing = snap.providers.filter(p => !p.configured).map(p => p.name);
  const settle = [];

  groups.replaceChildren(...active.map((p, i) => {
    const group = el("section", "group");
    const name = el("h2", "name", p.name);
    name.id = "provider-" + i;
    if (p.experimental) {
      const flag = el("span", "flag", " unverified");
      flag.title = "Copia's reading of this provider has not been confirmed against a live account.";
      name.append(flag);
    }
    group.setAttribute("aria-labelledby", name.id);
    group.append(name);
    if (p.error) {
      const trouble = el("div", "trouble");
      trouble.append(alertIcon(), el("p", null, p.error));
      trouble.title = p.error;
      group.append(trouble);
    } else {
      const bars = el("div", "bars");
      bars.append(...p.quotas.map((q, i) => quotaBar(p, q, i, snap, settle)));
      group.append(bars);
    }
    return group;
  }));
  // Bars were built at their previous level; let them ease to the new one.
  groups.offsetHeight;
  settle.forEach(run => run());

  const empty = $("empty");
  empty.hidden = active.length > 0;
  empty.textContent = snap.loading ? "Fetching quotas…" :
    "No signed-in coding tools found on this computer. Sign in with claude, codex, gemini or opencode, then refresh.";

  const when = snap.loading ? "refreshing…" : snap.updated_at ? "as of " +
    new Date(snap.updated_at * 1000).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" }) : "";
  if ($("when").textContent !== when) $("when").textContent = when;
  $("refresh").disabled = snap.loading;

  const notes = ["Bars show what is left."];
  if (snap.demo) notes.unshift("Demo data.");
  if (missing.length && active.length) notes.push("Not signed in: " + missing.join(", ") + ".");
  $("foot").textContent = notes.join(" ");

  // Tell assistive tech once per finished refresh, not on every repaint.
  if (!snap.loading && snap.updated_at !== announcedAt) {
    announcedAt = snap.updated_at;
    const problems = active.filter(p => p.error).map(p => `${p.name}: ${p.error}.`);
    $("announce").textContent = ["Quotas updated.", ...problems].join(" ");
  }

  // Measured synchronously: a hidden popover gets no animation frames.
  const box = document.body.getBoundingClientRect();
  const size = Math.ceil(box.width) + "x" + Math.ceil(box.height);
  if (size !== lastFit) {
    lastFit = size;
    invoke("fit", { width: Math.ceil(box.width), height: Math.ceil(box.height) });
  }
}

const load = () => invoke("get_snapshot").then(render);
$("refresh").addEventListener("click", () => invoke("refresh"));
document.addEventListener("keydown", e => {
  if (e.key === "Escape") invoke("hide");
  // Also keeps the webview from reloading the page on Ctrl/Cmd+R.
  if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "r") {
    e.preventDefault();
    invoke("refresh");
  }
});
listen("snapshot", load);
// Keep the "in 2h 5m" countdowns honest while the popover stays open.
setInterval(() => { if (last && !document.hidden) render(last); }, 60000);
load();
