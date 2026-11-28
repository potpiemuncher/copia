// Browser-only stand-in for the Tauri backend, so the popover can be designed
// by opening index.html from any static server. Inside the app Tauri has
// already defined window.__TAURI__, so this does nothing there. (It is a file
// rather than an inline script so the app's CSP can be script-src 'self'.)
// Add ?stress to the URL for long labels, a long error and enough providers to wrap.
(() => {
  if (window.__TAURI__) return;
  const now = Math.floor(Date.now() / 1000);
  const q = (label, used, secs) => ({ label, used, resets_at: now + secs });
  const p = (name, quotas, extra = {}) =>
    ({ id: name, name, configured: true, quotas, error: null, experimental: false, ...extra });
  const providers = [
    p("Claude", [q("5h", 0.38, 8900), q("week", 0.61, 259200)]),
    p("Codex", [q("5h", 0.12, 14400), q("week", 0.87, 432000)]),
    p("Antigravity", [
      q("gemini 5h", 0.08, 17800),
      q("gemini wk", 0.25, 518400),
      { label: "other 5h", used: 0.0, resets_at: null },
      q("other wk", 0.15, 432000),
    ]),
    p("OpenCode Go", [q("5h", 0.05, 3600), q("week", 0.44, 172800), q("month", 1, 900000)]),
    p("Cursor", [], { configured: false }),
  ];
  if (location.search.includes("stress")) {
    providers.push(
      p("Custom tool", [q("requests per day", 0.5, 600), q("x", 0.99, 60)]),
      p("Broken", [], { error: "network: https://example.invalid/api/v1/usage/summary?range=30d: Connection Failed: dns lookup failed for host example.invalid: no such host is known to this resolver at this time" }),
      ...["Alpha", "Beta", "Gamma", "Delta"].map(n => p(n, [q("5h", 0.3, 4000), q("week", 0.6, 90000), q("month", 0.2, 900000)])),
    );
  }
  const snapshot = { demo: true, loading: false, updated_at: now, warn_below: 0.5, low_below: 0.2, providers };
  window.__TAURI__ = {
    core: { invoke: async (cmd) => (cmd === "get_snapshot" ? snapshot : null) },
    event: { listen: async () => () => {} },
  };
})();
