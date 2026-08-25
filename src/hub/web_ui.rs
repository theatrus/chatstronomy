//! The hub's management page: one inline HTML document, no build pipeline.
//!
//! Structure mirrors the data model in two views: user-owned telescopes and
//! Discord delivery (servers, attachments, channels, and permissions). Every
//! control applies immediately — there are no save buttons — and
//! channels/roles are always picked from Discord data, never typed.
//!
//! Layout discipline: every card is head (title left, badges right) plus
//! labeled sections; every control row uses .controls with fixed control
//! heights so things line up.

/// The same multi-resolution Chatstronomy artwork used by the desktop app.
/// Embedding it keeps the Hub deployable as a single binary.
pub const FAVICON_ICO: &[u8] = include_bytes!("../../assets/branding/chatstronomy.ico");

pub const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Chatstronomy hub</title>
<link rel="icon" href="/favicon.ico" type="image/x-icon">
<style>
  :root {
    --bg: #0b0e14; --panel: #141922; --panel2: #1a2130; --control: #222938;
    --border: #2a3245; --control-border: #2a3245; --chip-on: #1b2740;
    --text: #e6edf3; --muted: #8b96a8; --accent: #58a6ff; --accent2: #1f6feb;
    --good: #3fb950; --bad: #f85149; --warn: #d29922; --radius: 10px;
    --telescope-wash: #0c1b2a; --telescope-active: #15304a; --telescope-edge: #3975ad;
    --delivery-wash: #1d142b; --delivery-active: #302044; --delivery-edge: #725ca1;
    --delivery-accent: #c4a7ff; --delivery-accent2: #6f42c1;
    --delivery-bg: #100c16; --delivery-panel: #17131f; --delivery-panel2: #241c30;
    --delivery-control: #2a2038; --delivery-border: #493968;
    --delivery-control-border: #8068ad;
    --ctl-h: 2.2rem;
  }
  * { box-sizing: border-box; }
  body {
    margin: 0; background: var(--bg); color: var(--text);
    font: 15px/1.55 system-ui, -apple-system, "Segoe UI", sans-serif;
  }
  .wrap { max-width: 820px; margin: 0 auto; padding: 1.5rem 1rem 4rem; }
  header {
    display: flex; align-items: center; flex-wrap: wrap;
    gap: .45rem .75rem; margin-bottom: 1.25rem;
  }
  header h1 { font-size: 1.25rem; margin: 0; flex: 1; letter-spacing: .01em; }
  #whoami { min-width: 0; margin-left: auto; overflow-wrap: anywhere; text-align: right; }
  .avatar { width: 28px; height: 28px; border-radius: 50%; vertical-align: middle; }
  a { color: var(--accent); text-decoration: none; }
  a:hover { text-decoration: underline; }

  .tabs {
    position: sticky; top: 0; z-index: 5; isolation: isolate;
    display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: .35rem;
    padding: .35rem; margin-bottom: .75rem; background: var(--panel);
    border: 1px solid var(--border); border-radius: var(--radius);
    box-shadow: 0 .55rem 1.2rem rgb(0 0 0 / 35%);
  }
  .tab {
    --tab-active: var(--telescope-active); --tab-edge: var(--telescope-edge);
    --tab-accent: var(--accent);
    display: flex; align-items: center; justify-content: center; gap: .5rem;
    min-height: 2.65rem; height: auto; background: #0e131c;
    border-color: var(--border); color: var(--muted); font-weight: 600;
  }
  #tab-delivery {
    --tab-active: var(--delivery-active); --tab-edge: var(--delivery-edge);
    --tab-accent: var(--delivery-accent);
  }
  .tab:hover { color: var(--text); border-color: var(--tab-edge); }
  .tab[aria-selected="true"] {
    color: var(--text); background: var(--tab-active); border-color: var(--tab-edge);
    box-shadow: inset 0 -3px 0 var(--tab-accent);
  }
  .tab:focus-visible { outline: 2px solid var(--tab-accent); outline-offset: 2px; }
  .tab-count {
    min-width: 1.45rem; padding: .05rem .4rem; border-radius: 999px;
    background: var(--bg); color: var(--muted); font-size: .72rem;
  }
  .tab[aria-selected="true"] .tab-count { color: var(--tab-accent); }
  .tab-panel {
    padding: .8rem; background: var(--view-wash); border: 1px solid var(--view-edge);
    border-radius: var(--radius);
  }
  #panel-telescopes { --view-wash: var(--telescope-wash); --view-edge: #284665; }
  #panel-delivery {
    --view-wash: var(--delivery-wash); --view-edge: #493968;
    --accent: var(--delivery-accent); --accent2: var(--delivery-accent2);
    --bg: var(--delivery-bg); --panel: var(--delivery-panel); --panel2: var(--delivery-panel2);
    --control: var(--delivery-control); --border: var(--delivery-border);
    --control-border: var(--delivery-control-border);
    --chip-on: color-mix(in srgb, var(--accent) 15%, var(--panel2));
  }
  .tab-panel > :last-child { margin-bottom: 0; }
  .tab-panel[hidden] { display: none; }

  /* Card anatomy: .head (title left, .badges right), then .section blocks. */
  .card {
    background: var(--panel); border: 1px solid var(--border);
    border-radius: var(--radius); padding: 1.1rem 1.2rem; margin-bottom: 1.1rem;
  }
  .sub {
    background: var(--panel2); border: 1px solid var(--border);
    border-radius: var(--radius); padding: .9rem 1rem; margin-top: .9rem;
  }
  .head {
    display: flex; align-items: center; gap: .55rem; min-height: 1.8rem;
    flex-wrap: wrap; row-gap: .45rem;
  }
  .head h2 { margin: 0; font-size: 1.05rem; }
  .head b { font-size: 1rem; }
  .head .badges { margin-left: auto; display: flex; gap: .4rem; align-items: center; flex-wrap: wrap; }
  .head .sub-note { color: var(--muted); font-size: .82rem; }

  .section { margin-top: .85rem; }
  .section > label {
    display: block; font-size: .72rem; color: var(--muted);
    margin-bottom: .35rem; text-transform: uppercase; letter-spacing: .06em;
  }
  .controls { display: flex; gap: .5rem; align-items: center; flex-wrap: wrap; }
  .controls + .controls, .chips + .controls { margin-top: .5rem; }

  .badge {
    font-size: .72rem; padding: .12rem .55rem; border-radius: 999px;
    border: 1px solid var(--border); color: var(--muted); white-space: nowrap;
  }
  .badge.good { color: var(--good); border-color: color-mix(in srgb, var(--good) 60%, transparent); }
  .badge.bad { color: var(--bad); border-color: color-mix(in srgb, var(--bad) 60%, transparent); }
  .badge.warn { color: var(--warn); border-color: color-mix(in srgb, var(--warn) 60%, transparent); }

  button {
    height: var(--ctl-h); background: var(--control); color: var(--text);
    border: 1px solid var(--control-border); border-radius: 7px; padding: 0 .9rem;
    cursor: pointer; font: inherit; font-size: .85rem;
    transition: border-color .12s, background .12s;
  }
  button:hover { border-color: var(--accent); }
  button:focus-visible, input:focus-visible, select:focus-visible,
  a:focus-visible, summary:focus-visible {
    outline: 2px solid var(--accent); outline-offset: 2px;
  }
  button:disabled { cursor: not-allowed; opacity: .55; }
  button:disabled:hover { border-color: var(--control-border); }
  button.primary { background: var(--accent2); border-color: var(--accent2); }
  button.primary:hover { filter: brightness(1.1); }
  button.subtle { background: transparent; color: var(--muted); }
  button.danger { color: var(--bad); }
  button.danger:hover { border-color: var(--bad); }

  input, select {
    height: var(--ctl-h); background: var(--bg); color: var(--text);
    border: 1px solid var(--control-border); border-radius: 7px; padding: 0 .55rem;
    font: inherit; font-size: .88rem;
  }
  select.pick { width: 230px; max-width: 100%; }
  input.name { width: 230px; max-width: 100%; }
  input.num { width: 6.2rem; }

  .chips { display: flex; flex-wrap: wrap; gap: .4rem; align-items: center; }
  .chip {
    display: inline-flex; align-items: center; gap: .35rem; height: 1.75rem;
    border: 1px solid var(--control-border); border-radius: 999px;
    padding: 0 .7rem; cursor: pointer; font-size: .82rem; color: var(--muted);
    user-select: none;
  }
  .chip input { display: none; }
  .chip.on {
    color: var(--text); border-color: var(--accent); background: var(--chip-on);
  }
  .chip .rm {
    background: none; border: none; padding: 0 0 0 .1rem; height: auto;
    color: var(--muted); cursor: pointer; font-size: .8rem;
  }
  .chip .rm:hover { color: var(--bad); }

  .token-box {
    background: var(--bg); border: 1px dashed var(--warn); border-radius: 8px;
    padding: .7rem .9rem .8rem; margin-top: .8rem; word-break: break-all;
    font-family: ui-monospace, monospace; font-size: .85rem;
  }
  .token-box .token-head {
    display: flex; align-items: center; justify-content: space-between;
    font-family: system-ui, sans-serif; color: var(--warn);
    margin-bottom: .35rem; gap: .5rem;
  }
  .token-box .token-head .rm {
    background: none; border: none; color: var(--muted); cursor: pointer;
    height: auto; padding: 0;
  }
  .token-box .token-head .rm:hover { color: var(--text); }
  .token-box .b-copy { margin-top: .45rem; }
  .token-box .hint {
    display: block; margin-top: .45rem;
    font-family: system-ui, sans-serif; word-break: normal;
  }
  .ico {
    width: 1em; height: 1em; vertical-align: -0.12em; margin-right: .4em;
    flex: none;
  }
  label .ico { width: 1.2em; height: 1.2em; vertical-align: -0.28em; }
  .hint { color: var(--muted); font-size: .8rem; }
  .next {
    display: flex; gap: .55rem; align-items: center; flex-wrap: wrap; margin-top: .8rem;
    border: 1px solid color-mix(in srgb, var(--accent) 45%, transparent);
    background: color-mix(in srgb, var(--accent) 10%, transparent);
    border-radius: 8px; padding: .5rem .8rem; font-size: .85rem;
  }
  .next .step { color: var(--accent); font-weight: 600; white-space: nowrap; }
  .next .next-copy { flex: 1; min-width: 14rem; }
  .next .b-open-delivery { margin-left: auto; }
  .footer-links {
    display: flex; gap: 1rem; justify-content: flex-end; margin-top: .7rem;
  }
  .footer-links a { color: var(--muted); font-size: .78rem; }
  .footer-links a:hover { color: var(--bad); text-decoration: none; }
  .legal-links {
    display: flex; gap: 1rem; justify-content: center; flex-wrap: wrap;
    margin-top: 1.1rem; padding-top: .8rem; border-top: 1px solid var(--border);
  }
  .legal-links a { color: var(--muted); font-size: .8rem; }
  .legal-links a:hover { color: var(--accent); }
  details.redeem { margin-top: .9rem; }
  details.redeem summary {
    cursor: pointer; color: var(--muted); font-size: .82rem; list-style: none;
  }
  details.redeem summary::before { content: "▸ "; }
  details.redeem[open] summary::before { content: "▾ "; }
  details.redeem summary:hover { color: var(--accent); }
  details.redeem .controls { margin-top: .5rem; }
  .steps {
    display: flex; gap: .4rem .9rem; flex-wrap: wrap; margin-top: .8rem;
    color: var(--muted); font-size: .85rem;
  }
  .steps b { color: var(--accent); font-weight: 600; }
  .error { color: var(--bad); margin: .5rem 0; }
  .banner {
    border: 1px solid var(--warn); background: color-mix(in srgb, var(--warn) 12%, transparent);
    border-radius: var(--radius); padding: .7rem 1rem; margin-bottom: 1rem;
    font-size: .88rem;
  }
  #toast {
    position: fixed; bottom: 1.1rem; left: 50%; transform: translateX(-50%);
    background: var(--panel2); border: 1px solid var(--border);
    border-radius: 8px; padding: .55rem 1.1rem; display: none; z-index: 10;
  }
  .sr-only {
    position: absolute; width: 1px; height: 1px; padding: 0; margin: -1px;
    overflow: hidden; clip: rect(0, 0, 0, 0); white-space: nowrap; border: 0;
  }
  @media (max-width: 520px) {
    .tabs { gap: .25rem; padding: .3rem; }
    .tab { gap: .3rem; padding-inline: .35rem; line-height: 1.25; }
    .tab-panel { padding: .4rem; }
  }
</style>
</head>
<body>
<div class="wrap">
  <header>
    <h1>Chatstronomy hub</h1>
    <span id="whoami"></span>
  </header>
  <div id="app"><p class="hint">Loading…</p></div>
  <footer class="legal-links" aria-label="Hosted-service policies">
    <a href="https://chatstronomy.com/hub-privacy.html" target="_blank" rel="noopener noreferrer">Hosted privacy</a>
    <a href="https://chatstronomy.com/hub-terms.html" target="_blank" rel="noopener noreferrer">Hosted terms</a>
  </footer>
</div>
<div id="toast"></div>
<div id="announcer" class="sr-only" role="status" aria-live="polite" aria-atomic="true"></div>
<script>
"use strict";
let CSRF = null;
let GUILDS = [];
let ACTIVE_TAB = "telescopes";
let RENDER_GENERATION = 0;
const TAB_ORDER = ["telescopes", "delivery"];
const HOSTED_PRIVACY_URL = "https://chatstronomy.com/hub-privacy.html";
const HOSTED_TERMS_URL = "https://chatstronomy.com/hub-terms.html";

const app = document.getElementById("app");
const esc = (s) => String(s ?? "").replace(/[&<>"']/g,
  (c) => ({"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;","'":"&#39;"}[c]));

function hostedPolicyLinks() {
  return '<a href="' + HOSTED_PRIVACY_URL + '" target="_blank" rel="noopener noreferrer">' +
    'hosted privacy statement</a> and <a href="' + HOSTED_TERMS_URL +
    '" target="_blank" rel="noopener noreferrer">hosted terms</a>';
}

function localPrivacyNotice() {
  return '<p class="hint">Ordinary event categories, images, and popup ' +
    'notifications start on by default. Review your N.I.N.A. event selections ' +
    'before pairing: disabled categories never reach this Hub, even for state ' +
    'reconstruction or command-failure reporting. Turning off images also blocks ' +
    'existing image history and thumbnails. Allowed equipment and status ' +
    'snapshots remain available, but Hub state may be incomplete. N.I.N.A. log ' +
    'forwarding, observatory location sharing, and hardware control start off. ' +
    'Hardware control requires the N.I.N.A. master switch plus individual ' +
    'command approvals.</p>';
}

function toast(msg) {
  const el = document.getElementById("toast");
  const announcer = document.getElementById("announcer");
  el.textContent = msg;
  el.style.display = "block";
  clearTimeout(el._t);
  el._t = setTimeout(() => { el.style.display = "none"; }, 3500);
  announcer.textContent = "";
  requestAnimationFrame(() => { announcer.textContent = msg; });
}

async function api(path, opts = {}) {
  opts.headers = Object.assign({}, opts.headers);
  if (opts.method && opts.method !== "GET") {
    opts.headers["x-csrf-token"] = CSRF;
    if (opts.body) opts.headers["content-type"] = "application/json";
  }
  const res = await fetch(path, opts);
  if (!res.ok) {
    throw new Error(await res.text() || res.statusText);
  }
  const type = res.headers.get("content-type") || "";
  return type.includes("json") ? res.json() : res.text();
}

async function boot() {
  const session = await api("/api/session");
  const who = document.getElementById("whoami");
  if (!session.authenticated) {
    who.innerHTML = "";
    app.innerHTML =
      '<div class="card"><h2 style="margin-top:0">Bring your observatory into Discord</h2>' +
      '<p>Add your telescopes, attach them to your servers, and let everyone ' +
      "watch sessions unfold — images, autofocus runs, guiding graphs, and " +
      "optional remote commands authorized locally in N.I.N.A.</p>" +
      localPrivacyNotice() +
      '<p><a href="/login"><button class="primary">Log in with Discord</button></a></p>' +
      '<p class="hint">Read the ' + hostedPolicyLinks() + ' before signing in.</p></div>';
    return;
  }
  CSRF = session.csrf_token;
  const u = session.user;
  who.innerHTML = (u.avatar_url ? '<img class="avatar" src="' + esc(u.avatar_url) + '" alt=""> ' : "") +
    esc(u.username) + ' &nbsp;·&nbsp; <a href="/logout">log out</a>';
  await renderAll();
}

function setActiveTab(name, focus = false) {
  if (!TAB_ORDER.includes(name)) name = "telescopes";
  ACTIVE_TAB = name;
  const tabs = [...app.querySelectorAll('[role="tab"]')];
  for (const tab of tabs) {
    const active = tab.dataset.tab === name;
    tab.setAttribute("aria-selected", String(active));
    tab.tabIndex = active ? 0 : -1;
    const panel = document.getElementById(tab.getAttribute("aria-controls"));
    if (panel) panel.hidden = !active;
    if (active && focus) tab.focus();
  }
}

function bindTabs() {
  const tabs = [...app.querySelectorAll('[role="tab"]')];
  tabs.forEach((tab, index) => {
    tab.onclick = () => setActiveTab(tab.dataset.tab);
    tab.onkeydown = (ev) => {
      let next = null;
      if (ev.key === "ArrowRight") next = (index + 1) % tabs.length;
      if (ev.key === "ArrowLeft") next = (index - 1 + tabs.length) % tabs.length;
      if (ev.key === "Home") next = 0;
      if (ev.key === "End") next = tabs.length - 1;
      if (next === null) return;
      ev.preventDefault();
      setActiveTab(tabs[next].dataset.tab, true);
    };
  });
  setActiveTab(ACTIVE_TAB);
}

async function renderAll(tab = ACTIVE_TAB, focusAttachmentId = null) {
  const generation = ++RENDER_GENERATION;
  ACTIVE_TAB = TAB_ORDER.includes(tab) ? tab : "telescopes";
  const hadShell = Boolean(app.querySelector(".tabs"));
  const previousScrollY = window.scrollY;
  app.setAttribute("aria-busy", "true");
  if (!hadShell) app.innerHTML = '<p class="hint">Loading…</p>';
  let guilds, mine;
  try {
    [guilds, mine] = await Promise.all([api("/api/guilds"), api("/api/telescopes")]);
  } catch (e) {
    if (generation !== RENDER_GENERATION) return;
    app.removeAttribute("aria-busy");
    if (hadShell) toast("Could not refresh: " + e.message);
    else app.innerHTML = '<p class="error">' + esc(e.message) + "</p>";
    return;
  }
  if (generation !== RENDER_GENERATION) return;
  GUILDS = guilds.guilds;
  app.innerHTML =
    '<div class="tabs" role="tablist" aria-label="Hub settings">' +
    '<button type="button" class="tab" role="tab" id="tab-telescopes" ' +
    'aria-controls="panel-telescopes" data-tab="telescopes">Telescopes ' +
    '<span class="tab-count">' + mine.telescopes.length + "</span></button>" +
    '<button type="button" class="tab" role="tab" id="tab-delivery" ' +
    'aria-controls="panel-delivery" data-tab="delivery">Discord delivery ' +
    '<span class="tab-count">' + GUILDS.length + "</span></button></div>" +
    '<section class="tab-panel" id="panel-telescopes" role="tabpanel" ' +
    'aria-labelledby="tab-telescopes"></section>' +
    '<section class="tab-panel" id="panel-delivery" role="tabpanel" ' +
    'aria-labelledby="tab-delivery"></section>';
  const telescopePanel = document.getElementById("panel-telescopes");
  const deliveryPanel = document.getElementById("panel-delivery");
  if (guilds.bot_configured === false) {
    const banner = document.createElement("div");
    banner.className = "banner";
    banner.innerHTML = "<b>This hub is running without a Discord bot token.</b> " +
      "Channel and role pickers, install checks, notifications, and slash commands " +
      "are all disabled until the hub operator sets <code>discord.bot_token</code> " +
      "and restarts the hub.";
    deliveryPanel.appendChild(banner);
  }
  renderMyTelescopes(mine.telescopes, telescopePanel);
  const attachmentRenders = GUILDS.map((g) => renderGuildCard(g, deliveryPanel));
  if (!GUILDS.length) {
    const note = document.createElement("div");
    note.className = "card";
    note.innerHTML = "<p>No servers where you hold <b>Manage Server</b>. " +
      "Ask a server admin, or check your Discord permissions.</p>";
    deliveryPanel.appendChild(note);
  }
  bindTabs();
  await Promise.all(attachmentRenders);
  if (generation !== RENDER_GENERATION) return;
  app.removeAttribute("aria-busy");
  if (hadShell) window.scrollTo(0, previousScrollY);
  if (focusAttachmentId !== null) {
    const row = [...app.querySelectorAll(".attachment")]
      .find((candidate) => candidate.dataset.id === String(focusAttachmentId));
    let control = row && row.querySelector(".b-addchan:not(:disabled)");
    if (!control && row) {
      const routes = [...row.querySelectorAll(".rm-route")];
      control = routes[routes.length - 1];
    }
    if (!control) control = document.getElementById("tab-delivery");
    if (control) control.focus({ preventScroll: true });
  }
}

// ---------- My telescopes ----------

function attachTargets(t) {
  // Registered, manageable servers this telescope is not attached to yet.
  const attached = new Set(t.attachments.map((a) => a.guild_id));
  return GUILDS.filter((g) => g.registered && !attached.has(g.id));
}

function nextStep(t, targets) {
  // One contextual cue per telescope: the single most useful next action.
  const hasChannels = t.attachments.some(
    (a) => Array.isArray(a.channels) && a.channels.length > 0
  );
  if (!t.attachments.length) {
    if (!targets.length) {
      return '<div class="next"><span class="step">Next</span>' +
        '<span class="next-copy">Set up a Discord server first, then return here to ' +
        'attach this telescope.</span><button type="button" class="b-open-delivery">' +
        "Open Discord delivery</button></div>";
    }
    return '<div class="next"><span class="step">Next</span>' +
      '<span class="next-copy">Attach this telescope to a server here, then choose ' +
      "its channels in Discord delivery.</span></div>";
  }
  if (!hasChannels) {
    return '<div class="next"><span class="step">Next</span>' +
      '<span class="next-copy">Choose at least one Discord channel so this telescope ' +
      'has somewhere to post.</span><button type="button" class="b-open-delivery">' +
      "Open Discord delivery</button></div>";
  }
  if (!t.connected) {
    return '<div class="next"><span class="step">Next</span>' +
      '<span class="next-copy">Connect your rig: get a pairing token and paste it into ' +
      "the N.I.N.A. plugin.</span></div>";
  }
  return "";
}

function rigBadge(connected) {
  return connected
    ? '<span class="badge good">rig online</span>'
    : '<span class="badge warn">rig offline</span>';
}

function controlBadge(connected, commandsEnabled) {
  if (!connected) return "";
  return commandsEnabled
    ? '<span class="badge good">N.I.N.A. commands approved</span>'
    : '<span class="badge warn">commands locked in N.I.N.A.</span>';
}

function renderMyTelescopes(telescopes, target) {
  const card = document.createElement("div");
  card.className = "card";
  let html = '<div class="head"><h2>' + ico("telescope") + "My telescopes</h2>" +
    '' +
    '<div class="badges"><span class="hint">yours across every server</span></div></div>' +
    localPrivacyNotice();
  if (!telescopes.length) {
    html += '<div class="steps"><span><b>1</b> Add a telescope</span>' +
      "<span><b>2</b> Attach it to a server</span>" +
      "<span><b>3</b> Pick its channels</span>" +
      "<span><b>4</b> Connect your rig</span></div>";
  }
  for (const t of telescopes) {
    const servers = t.attachments.length
      ? '<div class="chips">' + t.attachments.map((a) =>
          '<span class="chip on">' + esc(a.guild_name || a.guild_id) +
          (a.can_command ? "" : ' <span class="badge">feed only</span>') + "</span>").join("") +
        "</div>"
      : '<span class="hint">Not attached to any server yet.</span>';
    const targets = attachTargets(t);
    const attachControls = targets.length
      ? '<div class="controls"><select class="pick f-attach">' + targets.map((g) =>
          '<option value="' + esc(g.id) + '">' + esc(g.name) + "</option>").join("") +
        '</select><button class="b-attach">Attach to server</button></div>'
      : "";
    html +=
      '<div class="sub telescope" data-id="' + t.id + '">' +
      '<div class="head"><b>' + ico("telescope") + esc(t.name) + "</b>" + '' +
      '<div class="badges">' + rigBadge(t.connected) + controlBadge(t.connected, t.commands_enabled) +
      '<button class="' + (t.connected ? "subtle " : "") + 'b-token">' + ico("key") + "Connect rig…</button>" + '' +
      '<button class="subtle b-share">' + ico("share") + "Share…</button></div></div>" + '' +
      nextStep(t, targets) +
      '<div class="token-out"></div>' +
      '<div class="section"><label>' + ico("globe") + "Servers</label>" + '' + servers + attachControls + "</div>" +
      '<div class="section"><label>' + ico("clock") + "Image cooldown</label>" + '' +
      '<div class="controls"><input class="num f-cooldown" type="number" min="0" max="86400" value="' +
      t.image_cooldown_seconds + '"><span class="hint">Seconds between image posts — applies on change.</span>' +
      "</div></div>" +
      '<div class="footer-links">' +
      '<a href="javascript:;" class="b-revoke">Reset rig access</a>' +
      '<a href="javascript:;" class="b-delete">Delete telescope</a></div></div>';
  }
  html +=
    '<div class="controls" style="margin-top:.9rem">' +
    '<input class="name new-name" placeholder="telescope name (e.g. c925)">' +
    '<button class="primary b-create">Add telescope</button></div>';
  card.innerHTML = html;
  target.appendChild(card);

  card.querySelector(".b-create").onclick = async () => {
    const name = card.querySelector(".new-name").value.trim();
    if (!name) return;
    try {
      await api("/api/telescopes", { method: "POST", body: JSON.stringify({ name }) });
      toast("Telescope added");
      renderAll("telescopes");
    } catch (e) { toast(e.message); }
  };

  card.querySelectorAll(".telescope").forEach((row) => {
    const id = row.dataset.id;
    const openDelivery = row.querySelector(".b-open-delivery");
    if (openDelivery) {
      openDelivery.onclick = () => setActiveTab("delivery", true);
    }
    row.querySelector(".f-cooldown").onchange = async (ev) => {
      const body = { image_cooldown_seconds: parseInt(ev.target.value, 10) || 0 };
      try {
        await api("/api/telescopes/" + id, { method: "PATCH", body: JSON.stringify(body) });
        toast("Cooldown updated");
      } catch (e) { toast(e.message); }
    };
    const attach = row.querySelector(".b-attach");
    if (attach) {
      attach.onclick = async () => {
        const guild = row.querySelector(".f-attach").value;
        try {
          await api("/api/telescopes/" + id + "/attach",
            { method: "POST", body: JSON.stringify({ guild_id: guild }) });
          toast("Attached — choose a channel in Discord delivery");
          renderAll("delivery");
        } catch (e) { toast(e.message); }
      };
    }
    row.querySelector(".b-token").onclick = async () => {
      try {
        const out = await api("/api/telescopes/" + id + "/pairing-token", { method: "POST" });
        showToken(row, "key", "Pairing token — shown once, valid " +
          Math.round(out.expires_in_seconds / 60) + " minutes", out.token,
          "Paste into the N.I.N.A. plugin, then connect. " +
          "Issuing a new token cancels this one. Review the " +
          hostedPolicyLinks() + " before pairing.");
      } catch (e) { toast(e.message); }
    };
    row.querySelector(".b-share").onclick = async () => {
      try {
        const out = await api("/api/telescopes/" + id + "/share-code", { method: "POST" });
        showToken(row, "share", "Share code — single use, valid " +
          Math.round(out.expires_in_seconds / 86400) + " days", out.code,
          "Give this to a manager of another server. They redeem it on their server " +
          "card, against one of their channels. Their server gets the feed and read " +
          "commands; only servers you attach yourself can drive the telescope.");
      } catch (e) { toast(e.message); }
    };
    row.querySelector(".b-revoke").onclick = async () => {
      if (!confirm("Reset this telescope's rig access? The connected rig is cut off " +
        "and must pair again with a new token.")) return;
      try {
        await api("/api/telescopes/" + id + "/credentials", { method: "DELETE" });
        await api("/api/telescopes/" + id + "/pairing-tokens", { method: "DELETE" });
        toast("Rig access reset");
        renderAll("telescopes");
      } catch (e) { toast(e.message); }
    };
    row.querySelector(".b-delete").onclick = async () => {
      if (!confirm("Delete this telescope everywhere? All attachments, channels, " +
        "and rig credentials go with it.")) return;
      try {
        await api("/api/telescopes/" + id, { method: "DELETE" });
        toast("Telescope deleted");
        renderAll("telescopes");
      } catch (e) { toast(e.message); }
    };
  });
}

function ico(name) {
  const paths = {
    telescope: '<path d="M2.5 10.2 13 3.8l3.2 5.2L5.7 15.4Z"/><path d="M9.8 13.6 6.2 21"/><path d="M12.2 12.4 15.8 21"/><path d="M16.2 9l4.8-2"/>',
    key: '<path d="M21 2l-2 2m-7.61 7.61a5.5 5.5 0 1 1-7.778 7.778 5.5 5.5 0 0 1 7.777-7.777zm0 0L15.5 7.5m0 0l3 3L22 7l-3-3m-3.5 3.5L19 4"/>',
    share: '<circle cx="18" cy="5" r="3"/><circle cx="6" cy="12" r="3"/><circle cx="18" cy="19" r="3"/><line x1="8.59" y1="13.51" x2="15.42" y2="17.49"/><line x1="15.41" y1="6.51" x2="8.59" y2="10.49"/>',
    globe: '<circle cx="12" cy="12" r="10"/><line x1="2" y1="12" x2="22" y2="12"/><path d="M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z"/>',
    clock: '<circle cx="12" cy="12" r="10"/><polyline points="12 6 12 12 16 14"/>',
    zap: '<polygon points="13 2 3 14 12 14 11 22 21 10 12 10 13 2"/>',
  };
  return '<svg class="ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" ' +
    'stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">' +
    paths[name] + "</svg>";
}

function showToken(row, icon, title, value, hint) {
  // Render inside the telescope's own card, right where the click happened.
  document.querySelectorAll(".token-out").forEach((b) => { b.innerHTML = ""; });
  const box = row.querySelector(".token-out");
  box.innerHTML =
    '<div class="token-box"><div class="token-head"><span>' + ico(icon) + esc(title) +
    '</span><button type="button" class="rm b-close" title="Dismiss">✕</button></div>' +
    "<b>" + esc(value) + "</b><br>" +
    '<button class="b-copy">Copy</button><span class="hint">' + hint + "</span></div>";
  box.querySelector(".b-copy").onclick = () => {
    navigator.clipboard.writeText(value).then(() => toast("Copied"));
  };
  box.querySelector(".b-close").onclick = () => { box.innerHTML = ""; };
  box.scrollIntoView({ block: "nearest", behavior: "smooth" });
}

// ---------- Server cards ----------

function renderGuildCard(g, target) {
  const card = document.createElement("div");
  card.className = "card";
  const badges = [];
  if (g.registered) badges.push('<span class="badge good">registered</span>');
  if (g.bot_installed === true) badges.push('<span class="badge good">bot installed</span>');
  if (g.bot_installed === false) badges.push('<span class="badge bad">bot not installed</span>');
  const actions = [];
  if (g.bot_installed === false && g.install_url) {
    actions.push('<a href="' + esc(g.install_url) + '" target="_blank" rel="noopener">' +
      '<button class="primary">Add bot to server</button></a>');
  } else if (g.bot_installed === true && g.install_url) {
    actions.push('<a href="' + esc(g.install_url) + '" target="_blank" rel="noopener">' +
      '<button title="Re-authorize View Channel, Send Messages, Embed Links, and Attach Files">' +
      'Review bot permissions</button></a>');
  }
  if (!g.registered) {
    actions.push('<button class="primary b-register">Set up this server</button>');
  }
  card.innerHTML =
    '<div class="head"><h2>' + esc(g.name) + "</h2>" +
    '<div class="badges">' + badges.join("") + actions.join("") + "</div></div>" +
    '<div class="attachments"></div>';
  target.appendChild(card);
  const registerBtn = card.querySelector(".b-register");
  if (registerBtn) {
    registerBtn.onclick = async () => {
      try {
        await api("/api/guilds/" + g.id + "/register", { method: "POST" });
        toast("Server registered");
        renderAll("delivery");
      } catch (e) { toast(e.message); }
    };
  }
  if (g.registered) return renderAttachments(g, card.querySelector(".attachments"));
}

const POLICY_OPTIONS = [
  ["admins", "Server managers"],
  ["roles", "Managers + selected roles"],
  ["disabled", "Nobody (disabled)"],
];

function channelPicker(options, used, cls) {
  const free = options.channels.filter((c) => !used.includes(c.id));
  if (!options.channels.length) {
    const why = options.bot_configured === false
      ? "hub has no bot token"
      : "none visible to the bot";
    return '<select class="pick ' + cls + '" disabled><option value="">no channels — ' +
      why + "</option></select>";
  }
  if (!free.length) {
    return '<select class="pick ' + cls + '" disabled><option value="">all channels in use</option></select>';
  }
  return '<select class="pick ' + cls + '">' + free.map((c) =>
    '<option value="' + esc(c.id) + '">#' + esc(c.name) + "</option>").join("") + "</select>";
}

function channelChips(a) {
  if (!a.channels.length) {
    return '<span class="hint">No channels yet — this telescope is not posting here.</span>';
  }
  return '<div class="chips">' + a.channels.map((route) => {
    const name = route.channel_name ? '#' + route.channel_name : "channel " + route.channel_id;
    return '<span class="chip on">' + esc(name) +
      ' <button type="button" class="rm rm-route" data-route="' + route.route_id +
      '" aria-label="Remove ' + esc(name) + ' from delivery" title="Remove channel">' +
      "✕</button></span>";
  }).join("") + "</div>";
}

function roleChips(options, selected) {
  if (!options.roles.length) {
    return '<span class="hint">No assignable roles in this server.</span>';
  }
  return '<div class="chips">' + options.roles.map((r) => {
    const on = selected.includes(r.id);
    return '<label class="chip' + (on ? " on" : "") + '"><input type="checkbox" value="' +
      esc(r.id) + '"' + (on ? " checked" : "") + ">@" + esc(r.name) + "</label>";
  }).join("") + "</div>";
}

async function renderAttachments(g, el) {
  let data, options;
  try {
    [data, options] = await Promise.all([
      api("/api/guilds/" + g.id + "/attachments"),
      api("/api/guilds/" + g.id + "/options"),
    ]);
  } catch (e) { el.innerHTML = '<p class="error">' + esc(e.message) + "</p>"; return; }

  const usedChannels = data.attachments.flatMap((a) => a.channels.map((r) => r.channel_id));
  let html = "";
  for (const a of data.attachments) {
    const canAddChannel = options.channels.some((c) => !usedChannels.includes(c.id));
    const badges =
      rigBadge(a.connected) +
      (a.can_command ? controlBadge(a.connected, a.commands_enabled)
                     : '<span class="badge">feed only</span>');
    const owner = a.owned_by_me ? "" :
      '<span class="sub-note">shared by ' + esc(a.owner_name) + "</span>";
    const consentHint = a.connected && !a.commands_enabled
      ? "Remote control is off in N.I.N.A. The telescope owner must enable the master switch and approve each command individually."
      : "Only commands individually approved in N.I.N.A. are available; server permissions cannot grant additional access.";
    const commands = a.can_command
      ? '<div class="section"><label>' + ico("zap") + "Commands — who may drive this telescope here</label>" + '' +
        '<div class="controls"><select class="pick f-policy">' +
        POLICY_OPTIONS.map(([v, label]) =>
          '<option value="' + v + '"' + (a.write_policy === v ? " selected" : "") + ">" +
          label + "</option>").join("") +
        '</select><span class="hint">Applies on change.</span></div>' +
        '<div class="roles-field" style="margin-top:.5rem' +
        (a.write_policy === "roles" ? "" : ";display:none") + '">' +
        roleChips(options, a.allowed_role_ids) + "</div>" +
        '<p class="hint">' + consentHint + "</p></div>"
      : '<div class="section"><label>' + ico("zap") + "Commands</label>" + '' +
        '<span class="hint">This server receives the feed and read commands only.</span></div>';
    html +=
      '<div class="sub attachment" data-id="' + a.attachment_id + '">' +
      '<div class="head"><b>' + ico("telescope") + esc(a.telescope_name) + "</b>" + owner +
      '<div class="badges">' + badges +
      '<button class="subtle danger b-detach">Detach</button></div></div>' +
      '<div class="section"><label># Channels — where this telescope posts</label>' +
      channelChips(a) +
      '<div class="controls">' + channelPicker(options, usedChannels, "f-addchan") +
      '<button class="b-addchan"' + (canAddChannel ? "" : " disabled") +
      ">Add channel</button></div></div>" +
      commands +
      "</div>";
  }
  if (!data.attachments.length) {
    html += '<p class="hint" style="margin:.8rem 0 0">No telescopes here yet. Attach one of ' +
      "yours from “My telescopes”, or redeem a share code below.</p>";
  }
  html +=
    '<details class="redeem"><summary>Have a share code from another server’s ' +
    "telescope owner? Redeem it here</summary>" +
    '<div class="controls">' +
    '<input class="name share-code" placeholder="share code (cssh_…)">' +
    channelPicker(options, usedChannels, "sub-channel") +
    '<button class="b-subscribe">Subscribe</button></div></details>';
  el.innerHTML = html;

  el.querySelector(".b-subscribe").onclick = async () => {
    const code = el.querySelector(".share-code").value.trim();
    const channelBox = el.querySelector(".sub-channel");
    if (!code) { toast("Enter a share code"); return; }
    if (channelBox.disabled || !channelBox.value) { toast("Pick a channel"); return; }
    try {
      const out = await api("/api/guilds/" + g.id + "/subscribe",
        { method: "POST", body: JSON.stringify({ code, channel_id: channelBox.value }) });
      toast("Subscribed to " + out.telescope_name);
      renderAll("delivery");
    } catch (e) { toast(e.message); }
  };

  el.querySelectorAll(".attachment").forEach((row) => {
    const id = row.dataset.id;
    const rolesField = row.querySelector(".roles-field");
    const policy = row.querySelector(".f-policy");

    const savePermissions = async () => {
      const roles = [...row.querySelectorAll(".chip input:checked")].map((box) => box.value);
      try {
        await api("/api/attachments/" + id, {
          method: "PATCH",
          body: JSON.stringify({ write_policy: policy.value, allowed_role_ids: roles }),
        });
        toast("Permissions updated");
      } catch (e) { toast(e.message); }
    };
    if (policy) {
      policy.onchange = () => {
        if (rolesField) rolesField.style.display = policy.value === "roles" ? "" : "none";
        savePermissions();
      };
    }
    row.querySelectorAll(".chip input").forEach((box) => {
      box.onchange = () => {
        box.closest(".chip").classList.toggle("on", box.checked);
        savePermissions();
      };
    });

    row.querySelector(".b-addchan").onclick = async () => {
      const box = row.querySelector(".f-addchan");
      if (box.disabled || !box.value) return;
      try {
        await api("/api/attachments/" + id + "/channels",
          { method: "POST", body: JSON.stringify({ channel_id: box.value }) });
        toast("Channel added");
        renderAll("delivery", id);
      } catch (e) { toast(e.message); }
    };
    row.querySelectorAll(".rm-route").forEach((btn) => {
      btn.onclick = async () => {
        try {
          await api("/api/attachments/" + id + "/channels/" + btn.dataset.route,
            { method: "DELETE" });
          toast("Channel removed");
          renderAll("delivery", id);
        } catch (e) { toast(e.message); }
      };
    });
    row.querySelector(".b-detach").onclick = async () => {
      if (!confirm("Detach this telescope from this server? Its channels here are removed.")) return;
      try {
        await api("/api/attachments/" + id, { method: "DELETE" });
        toast("Detached");
        renderAll("delivery");
      } catch (e) { toast(e.message); }
    };
  });
}

boot().catch((e) => { app.innerHTML = '<p class="error">' + esc(e.message) + "</p>"; });
</script>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_has_the_expected_hooks() {
        // The page drives these endpoints; renaming one must fail a test.
        for needle in [
            "/api/session",
            "/api/guilds",
            "/api/telescopes",
            "/attachments",
            "/options",
            "/pairing-token",
            "/share-code",
            "/subscribe",
            "/login",
            "/logout",
            "x-csrf-token",
        ] {
            assert!(INDEX_HTML.contains(needle), "missing {needle}");
        }
        assert!(INDEX_HTML.contains("<h1>Chatstronomy hub</h1>"));
        assert!(!INDEX_HTML.to_ascii_lowercase().contains("space | cat"));
    }

    #[test]
    fn page_reflects_the_ownership_model() {
        for needle in [
            "My telescopes",
            "Attach to server",
            "feed only",
            "N.I.N.A. commands approved",
            "Detach",
        ] {
            assert!(INDEX_HTML.contains(needle), "missing {needle}");
        }
    }

    #[test]
    fn page_makes_the_local_control_boundary_visible() {
        for needle in [
            "function controlBadge(connected, commandsEnabled)",
            "t.commands_enabled",
            "a.commands_enabled",
            "commands locked in N.I.N.A.",
            "hardware control start off",
            "Hardware control requires the N.I.N.A. master switch plus individual",
            "command approvals",
            "enable the master switch and approve each command individually",
            "Only commands individually approved in N.I.N.A. are available",
        ] {
            assert!(INDEX_HTML.contains(needle), "missing {needle}");
        }
    }

    #[test]
    fn page_explains_the_local_event_and_privacy_boundary() {
        for needle in [
            "function localPrivacyNotice()",
            "Ordinary event categories, images, and popup",
            "notifications start on by default",
            "before pairing: disabled categories never reach this Hub",
            "reconstruction or command-failure reporting",
            "Turning off images also blocks",
            "existing image history and thumbnails",
            "snapshots remain available, but Hub state may be incomplete",
            "forwarding, observatory location sharing, and hardware control start off",
        ] {
            assert!(INDEX_HTML.contains(needle), "missing {needle}");
        }
        assert!(INDEX_HTML.matches("localPrivacyNotice()").count() >= 3);
    }

    #[test]
    fn hosted_policies_are_visible_before_login_and_pairing() {
        assert!(INDEX_HTML.contains("https://chatstronomy.com/hub-privacy.html"));
        assert!(INDEX_HTML.contains("https://chatstronomy.com/hub-terms.html"));
        assert!(INDEX_HTML.contains("aria-label=\"Hosted-service policies\""));
        assert!(INDEX_HTML.contains("before signing in"));
        assert!(INDEX_HTML.contains("before pairing"));
        assert!(INDEX_HTML.matches("hostedPolicyLinks()").count() >= 3);
    }

    #[test]
    fn page_uses_accessible_telescope_and_delivery_tabs() {
        for needle in [
            "Hub settings",
            "tab-telescopes",
            "panel-telescopes",
            "tab-delivery",
            "panel-delivery",
            "Discord delivery",
            "aria-selected",
            "ArrowRight",
            "ArrowLeft",
            "Home",
            "End",
        ] {
            assert!(INDEX_HTML.contains(needle), "missing {needle}");
        }
        assert_eq!(INDEX_HTML.matches("class=\"tab\" role=\"tab\"").count(), 2);
        assert_eq!(INDEX_HTML.matches("role=\"tabpanel\"").count(), 2);
    }

    #[test]
    fn page_keeps_distinct_tab_workspaces_visible_while_scrolling() {
        for needle in [
            "position: sticky; top: 0",
            "--telescope-active:",
            "--delivery-active:",
            "#panel-telescopes { --view-wash:",
            "#panel-delivery {",
        ] {
            assert!(INDEX_HTML.contains(needle), "missing {needle}");
        }
    }

    #[test]
    fn page_scopes_delivery_controls_to_the_delivery_chroma() {
        for needle in [
            "--delivery-accent2:",
            "--accent: var(--delivery-accent)",
            "--accent2: var(--delivery-accent2)",
            "--panel: var(--delivery-panel)",
            "--panel2: var(--delivery-panel2)",
            "--control: var(--delivery-control)",
            "--border: var(--delivery-border)",
            "--control-border: var(--delivery-control-border)",
            "--chip-on: color-mix(in srgb, var(--accent) 15%, var(--panel2))",
        ] {
            assert!(INDEX_HTML.contains(needle), "missing {needle}");
        }
        assert!(INDEX_HTML.contains("--chip-on: #1b2740"));
        assert!(INDEX_HTML.contains("background: var(--chip-on)"));
    }

    #[test]
    fn page_links_the_hub_favicon() {
        assert!(
            INDEX_HTML.contains("<link rel=\"icon\" href=\"/favicon.ico\" type=\"image/x-icon\">")
        );
        assert!(FAVICON_ICO.starts_with(&[0, 0, 1, 0]));
    }

    #[test]
    fn channel_onboarding_uses_routes_and_refreshes_both_views() {
        assert!(INDEX_HTML.contains("Array.isArray(a.channels) && a.channels.length > 0"));
        assert!(INDEX_HTML.contains("if (!targets.length)"));
        assert!(INDEX_HTML.contains("Set up a Discord server first"));
        assert!(INDEX_HTML.contains("b-open-delivery"));
        assert!(INDEX_HTML.contains("let RENDER_GENERATION = 0"));
        assert!(
            INDEX_HTML
                .matches("generation !== RENDER_GENERATION")
                .count()
                >= 2
        );
        assert!(INDEX_HTML.contains(".b-addchan:not(:disabled)"));
        assert!(INDEX_HTML.contains("canAddChannel ? \"\" : \" disabled\""));
        assert!(INDEX_HTML.contains("aria-label=\"Remove "));
        assert!(
            INDEX_HTML.contains("toast(\"Channel added\");\n        renderAll(\"delivery\", id);")
        );
        assert!(
            INDEX_HTML
                .contains("toast(\"Channel removed\");\n          renderAll(\"delivery\", id);")
        );
    }

    #[test]
    fn mutation_status_is_announced_accessibly() {
        assert!(INDEX_HTML.contains("role=\"status\""));
        assert!(INDEX_HTML.contains("aria-live=\"polite\""));
        assert!(INDEX_HTML.contains("announcer.textContent = msg"));
    }

    #[test]
    fn installed_bots_can_reauthorize_attachment_permissions() {
        assert!(INDEX_HTML.contains("Review bot permissions"));
        assert!(INDEX_HTML.contains("Attach Files"));
    }

    #[test]
    fn page_applies_settings_instantly() {
        // One interaction model: everything applies on change; no split
        // between instant channels and save-button permissions.
        assert!(!INDEX_HTML.contains("Save permissions"));
        assert!(!INDEX_HTML.contains(">Save<"));
        assert!(INDEX_HTML.contains("applies on change"));
        assert!(INDEX_HTML.contains("savePermissions()"));
    }

    #[test]
    fn page_uses_pickers_not_id_inputs() {
        // Channels and roles are picked from Discord data, never typed.
        assert!(INDEX_HTML.contains("f-addchan"));
        assert!(INDEX_HTML.contains("chips"));
        assert!(!INDEX_HTML.contains("placeholder=\"channel id\""));
        assert!(!INDEX_HTML.contains("placeholder=\"role ids\""));
    }

    #[test]
    fn page_offers_all_write_policies() {
        for policy in ["admins", "roles", "disabled"] {
            assert!(INDEX_HTML.contains(policy), "missing policy {policy}");
        }
    }

    #[test]
    fn tokens_render_inside_their_telescope_card() {
        // Pairing tokens and share codes appear in the clicked telescope's
        // own sub-card, not in one shared box below every telescope.
        assert!(INDEX_HTML.contains("function showToken(row"));
        assert!(INDEX_HTML.contains("token-head"));
        assert!(INDEX_HTML.contains("b-close"));
        // Exactly one token-out container, inside the telescope template.
        assert_eq!(INDEX_HTML.matches("class=\"token-out\"").count(), 1);
    }

    #[test]
    fn page_surfaces_missing_bot_token() {
        // A hub without a bot token must say so, not just look broken.
        assert!(INDEX_HTML.contains("bot_configured"));
        assert!(INDEX_HTML.contains("without a Discord bot token"));
    }
}
