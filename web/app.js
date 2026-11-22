"use strict";
/* global h, s, fmt, fmtFull, pct, clock, timer, lineChart, timelineChart, stackedColumns, segmentedBar, legend, Tip */

const SERIES = [1, 2, 3, 4, 5, 6, 7, 8].map(i => `var(--series-${i})`);
const OTHER = "var(--other)";
// hues for the timeline lanes: one per ability (its effects share it), then one per kind of effect
const ABILITY_HUES = ["var(--series-7)", "var(--series-5)", "var(--series-6)"];
const EFFECT_HUES = { skill: "var(--series-3)", mod: "var(--series-4)", ext: "var(--series-1)", debuff: "var(--series-8)", other: "var(--muted)" };

const ui = {
  list: [], live: null, version: -1,
  sel: null,            // encounter id, or null = follow the latest
  about: false,         // the About page is shown instead of an encounter
  records: false,       // the Records page is shown instead of an encounter
  recordsData: null,    // what /api/records gave, null until it has
  recordsVersion: null, // game version the records are for: an id, "all", or null for the one being played
  recordsTab: "team_dps", // which record the Records page shows
  detail: null,
  detailText: "",       // raw JSON of `detail`, to skip re-rendering when a refresh brings nothing new
  stale: false,         // a refresh arrived while a chart or card was being read
  player: null,         // selected client id
  tab: "sources",
  mode: "dps",          // dps | cumulative
  window: 3,            // rolling window (s) for the DPS line
  target: null,         // boss entity id the chart is filtered to (null = all targets)
  srcColors: {},        // encounter id -> player cid -> source key -> slot (color follows the source, not its rank)
  lastFetch: 0,
  notice: "",           // what the last import did, shown above the page for a while
  noticeTimer: 0,
};

// ---------- data ----------
async function getJSON(url, opts) {
  const r = await fetch(url, opts);
  if (!r.ok) throw new Error(`${url}: ${r.status}`);
  return r.json();
}
// A request that changes or does something. The header marks it as coming from the app's own page: the server
// refuses such requests without it, and a page of another site cannot send it here.
function act(url, method) { return getJSON(url, { method: method || "POST", headers: { "X-Zar-Logs": "1" } }); }
function notify(text) {
  ui.notice = text;
  clearTimeout(ui.noticeTimer);
  ui.noticeTimer = setTimeout(() => { ui.notice = ""; renderMain(); }, 12000);
  renderMain();
}
async function refreshList() {
  ui.list = await getJSON("/api/encounters");
  renderList();
  if (!currentId() && ui.detail) { ui.detail = null; renderMain(); }
}
function currentId() {
  if (ui.sel) return ui.sel;
  return ui.list.length ? ui.list[0].id : null;
}
async function refreshDetail(force) {
  const id = currentId();
  if (!id) { ui.detail = null; renderMain(); return; }
  if (!force && ui.detail && ui.detail.id === id && ui.detail.closed) return;
  try {
    const r = await fetch(`/api/encounter/${encodeURIComponent(id)}`);
    if (!r.ok) throw new Error(String(r.status));
    const text = await r.text();
    if (text === ui.detailText && ui.detail && ui.detail.id === id) return;   // nothing new: leave the page alone
    const d = JSON.parse(text);
    const changed = !ui.detail || ui.detail.id !== d.id;
    ui.detail = d;
    ui.detailText = text;
    if (changed) ui.player = null;
    if (changed) renderMain({ top: true });
    else if (!window.chartHover && !window.cardHover) renderMain();
    else ui.stale = true;                       // being read right now: redraw when the pointer leaves
  } catch (e) {
    if (ui.detail && ui.detail.id !== id) { ui.detail = null; renderMain(); }
  }
}
function onLive(l) {
  ui.live = l;
  const st = document.getElementById("status");
  const fighting = l.encounter && !l.encounter.closed && l.status.state === "attached";
  st.className = "status " + (fighting ? "live" : l.status.state);
  st.querySelector(".status-text").textContent =
    l.status.state === "attached" ? (fighting ? "Attached · in mission" : l.place ? "Attached · in " + l.place : "Attached to the game") :
    l.status.state === "waiting" ? "Waiting for the game" :
    l.status.state === "error" ? "Capture error: " + (l.status.error || "see the console") : "Starting";
  // the app reads names, pictures and the memory layout from the game's files on first start and after a game update
  if (l.prep) st.querySelector(".status-text").textContent += " · " + l.prep;
  // the app has an in-game overlay it can show and hide
  const ov = document.getElementById("overlay-toggle");
  if (ov && l.overlay != null) {
    ov.hidden = false;
    ov.textContent = l.overlay ? "Overlay: on" : "Overlay: off";
    ov.setAttribute("aria-pressed", String(!!l.overlay));
    ov.onclick = () => act("/api/overlay/toggle").catch(() => {});
  }
  if (l.version !== ui.version) { ui.version = l.version; refreshList().then(() => refreshDetail(true)); return; }
  const id = currentId();
  const liveId = l.encounter && !l.encounter.closed ? l.encounter.id : null;
  if (liveId && !ui.list.some(r => r.id === liveId)) { refreshList().then(() => refreshDetail(true)); return; }
  if (id && id === liveId && Date.now() - ui.lastFetch > 1900) { ui.lastFetch = Date.now(); refreshDetail(true); refreshListQuiet(); }
}
async function refreshListQuiet() { try { ui.list = await getJSON("/api/encounters"); renderList(); } catch (e) { /* next tick */ } }

// ---------- logs recorded by earlier versions ----------
// The format of a log from which its kills are the game's kills, its damage-taken rows are one per attacker, and a
// skill is only named as an effect's source for a player who has it.
const TIDY_FORMAT = 10;
// A log from before can name a passive skill as the source of an effect on a player who never took it (an enemy's
// Vulnerable shown as "Skill: Threat Identification"). The build in the log says which skills the player has: a
// skill it lists at rank 0 is not the player's, and is a teammate's only if a teammate has it.
function skillRanks(p) {
  const ranks = new Map();
  for (const g of (p.build && p.build.skills) || []) for (const sk of g.items || []) ranks.set(sk.name, sk.rank);
  return ranks;
}
function playerLanes(d, p) {
  const lanes = (p && p.lanes) || [];
  if (!p || d.format >= TIDY_FORMAT) return lanes;
  const mine = skillRanks(p);
  return lanes.map(ln => {
    const m = /^Skill: (.+)$/.exec(ln.source || "");
    if (!m || mine.get(m[1]) !== 0) return ln;
    const ally = d.players.some(q => q !== p && skillRanks(q).get(m[1]) > 0);
    return Object.assign({}, ln, { source: ally ? "Ally: " + m[1] : "" });
  });
}
// A player's rows, by where each effect comes from: their abilities (each with the effects it gives), their skills,
// their mods, everyone and everything else, and the debuffs on them. An effect the meter could not place is last.
// `quiet` rows (trigger carriers, effects on for an instant) are only listed in the chart's tooltip while active.
function playerRows(d, p) {
  if (!p) return [];
  const lanes = playerLanes(d, p);
  const whose = p.local ? "your" : `${p.name}'s`, on = p.local ? "you" : p.name;
  const abilities = new Set(lanes.filter(l => l.kind === "ability").map(l => l.name));
  const cats = { abil: [], skill: [], mod: [], ext: [], debuff: [], other: [] };
  const later = new Map();     // ability name -> its effects that were filed with the other effects
  for (const ln of lanes) {
    const row = Object.assign({}, ln, { quiet: ln.group === "Other effects" });
    const src = ln.source || "";
    let m;
    if (ln.group === "Abilities") cats.abil.push(row);
    else if (abilities.has(src)) {
      if (ln.name.startsWith(src + ": ")) row.name = ln.name.slice(src.length + 2);
      row.indent = 1;
      if (!later.has(src)) later.set(src, []);
      later.get(src).push(row);
    } else if (ln.group === "Debuffs") cats.debuff.push(row);
    else if ((m = /^Skill: (.+)$/.exec(src))) cats.skill.push(Object.assign(row, { source: m[1] }));
    else if ((m = /^Mod: (.+)$/.exec(src))) cats.mod.push(Object.assign(row, { source: m[1] }));
    else if (/^Ally: /.test(src) || ln.from === "other" || ln.from === "ally" || ln.group === "Buffs") cats.ext.push(row);
    else cats.other.push(row);
  }
  // each ability with its effects under it, in one hue
  const abil = [];
  let cur = null, ai = -1;
  const flush = () => { if (cur && later.has(cur)) abil.push(...later.get(cur)); };
  for (const ln of cats.abil) {
    if (ln.kind === "ability") { flush(); cur = ln.name; }
    abil.push(ln);
  }
  flush();
  for (const ln of abil) { if (ln.kind === "ability") ai++; ln.color = ABILITY_HUES[Math.max(ai, 0) % ABILITY_HUES.length]; }
  const groups = [["abil", `Buffs from ${whose} abilities`, abil], ["skill", `Buffs from ${whose} skills`, cats.skill],
    ["mod", `Buffs from ${whose} mods`, cats.mod], ["ext", "External buffs", cats.ext], ["debuff", `Debuffs on ${on}`, cats.debuff],
    ["other", `Other effects on ${on}`, cats.other]];
  return groups.flatMap(([g, label, rows]) => rows.map(r => Object.assign(r, { group: g, groupLabel: label, color: r.color || EFFECT_HUES[g] })));
}
// The effects on a boss, each with the player who put it there, in that player's colour. Logs from 0.3.5 on name
// the player and keep one row per player; before, the giver entity is matched against the players' own effects.
function bossRows(d, b) {
  const byGiver = new Map();
  for (const q of d.players) for (const ln of q.lanes || []) if (ln.from === "self" && ln.giver) byGiver.set(ln.giver, q);
  const rows = (b.lanes || []).map(ln => {
    const q = ln.by_cid != null ? d.players.find(x => x.cid === ln.by_cid) : byGiver.get(ln.giver);
    let source = ln.source || "";
    if (q && ln.by_name == null) source = source ? `${q.name} · ${source}` : q.name;
    const debuff = ln.group === "Debuffs";
    return Object.assign({}, ln, { source, name: q ? `${ln.name} · ${q.name}` : ln.name, quiet: ln.group === "Other effects",
      group: debuff ? "b-debuff" : "b-other", groupLabel: (debuff ? "Debuffs on " : "Other effects on ") + b.name,
      color: q ? playerColor(d, q.cid) : debuff ? EFFECT_HUES.debuff : EFFECT_HUES.other });
  });
  return rows.filter(r => r.group === "b-debuff").concat(rows.filter(r => r.group !== "b-debuff"));
}
// A crit rate is over the hits of the player's weapons: a turret, an ability or a barrel does not crit, and counting
// its hits would only water the rate down. A log from before weapon hits were counted apart gives them through the
// player's weapon sources; a target row of such a log only has all hits.
function critRate(x) {
  if (x.weapon_hits != null) return x.weapon_hits ? x.weapon_crits / x.weapon_hits : 0;
  const weapons = (x.sources || []).filter(src => src.kind === "weapon");
  if (weapons.length) {
    const hits = weapons.reduce((a, src) => a + src.hits, 0);
    return hits ? weapons.reduce((a, src) => a + src.crits, 0) / hits : 0;
  }
  return x.hits ? x.crits / x.hits : 0;
}
// Kills in the tables of such a log are final blows, which counts an enemy that splits (Multipliers) at every split.
function oldKillsNote(d, p, rows) {
  const sum = rows.reduce((a, x) => a + (x.kills || 0), 0);
  if (d.format >= TIDY_FORMAT || !p.kills_native || sum <= p.kills_native) return null;
  return h("div", { class: "note" }, `Recorded by an earlier version: "Kills" in this table counts final blows (${fmtFull(sum)}), `
    + `and an enemy that splits is counted at every split. The game's own count, ${fmtFull(p.kills_native)}, is the one shown above.`);
}

// ---------- sidebar ----------
function missionName(m, fallback) { return (m && m.name) || fallback || "Mission"; }
// The game mode and difficulty say what kind of run it was; the map comes second. "Pods (Hard)", then "Geode Cavern".
function isTough(m) { return !!(m && (m.tough || /Tough/.test(m.difficulty || ""))); }
function diffName(m) { return ((m && m.difficulty) || "").replace(/Tough/, "").replace(/[ ·]+$/, "").trim(); }
function missionTitle(m) {
  const bits = [diffName(m), isTough(m) ? "Tough" : ""].filter(Boolean).join(", ");
  if (m && m.type) return m.type + (bits ? ` (${bits})` : "");
  return missionName(m);
}
// difficulty the way the game writes it on a mission card: EASY / MEDIUM / HARD, and TOUGH for the modifier
function diffTags(m) {
  const d = diffName(m), out = [];
  if (d) out.push(h("span", { class: "diff d-" + d.toLowerCase() }, d));
  if (isTough(m)) out.push(h("span", { class: "diff d-tough" }, "Tough"));
  return out;
}
// How strong the game made the mission's enemies. Two things, both read in the game's code: the mission level (the
// players' average level rounded down, plus what Tough adds) picks a point on the game's difficulty curve, and the
// number of players multiplies that (the game's own table, ×1.7 for two players and so on). So a low-level player
// lowers the level, and every player adds to the team multiplier. The chip shows the one number the game scales
// enemy stats by, the two together ("21.7" for level 49 and ×2.5); the tooltip has the parts. It sits on the line
// under a title (the map's line), not among the title's tags, so a long title never pushes it off.
function levelTag(m) {
  if (!m || !(m.level > 0)) return null;
  const sc = m.scaling || {};
  const lines = [`Mission level ${m.level}: the players' average level, rounded down${isTough(m) ? ", plus what Tough adds" : ""}`
    + (m.level_from === "computed" ? " (worked out from the players' levels)" : " (the game's own value)") + "."];
  if (sc.enemies) lines.unshift(`Difficulty ${num1(sc.enemies)}: what the game scaled this mission's enemies by.`);
  if (sc.team) {
    lines.push(`Enemies are scaled by the game's difficulty curve at that level (×${num2(sc.level)}) times ×${num2(sc.team)} for `
      + `${sc.players === 1 ? "a solo player" : `a team of ${sc.players}`}: ×${num2(sc.enemies)} in all.`
      + (sc.from === "computed" ? " Worked out from the game's tables; newer logs have the game's own numbers." : " The game's own numbers for this mission."));
    lines.push("Bosses and some enemies have their own per-player table on top of this.");
  }
  // without the game's tables (and an older log) there is only the level to show
  const text = sc.enemies ? num1(sc.enemies) : `Lvl ${m.level}`;
  return h("span", { class: "diff d-level", title: lines.join("\n") }, text);
}
function num1(x) { return x == null ? "?" : x.toFixed(1); }
function num2(x) { return x == null ? "?" : (Math.round(x * 100) / 100).toString(); }
function artStyle(m) { return m && m.art ? `background-image:url("${m.art}")` : ""; }
function missionPlace(m) { return m && m.type ? m.name || "" : ""; }
// "0:27 (0:42)": mission time, then the game's own timer, which also counts the intro and outro cutscenes
function times(r) { return timer(r.duration) + (r.game_time ? ` (${timer(r.game_time)})` : ""); }
function renderList() {
  const rb = document.getElementById("records-open");
  if (rb) rb.classList.toggle("on", ui.records);
  const host = document.getElementById("enc-list");
  const id = currentId();
  if (!ui.list.length) {
    host.replaceChildren(h("div", { class: "empty" }, "No encounters yet. Start a mission and deal some damage; it appears here on its own."));
    return;
  }
  host.replaceChildren(...ui.list.map(r => {
    const d = new Date(r.start * 1000);
    const m = r.mission || {};
    return h("button", { class: "enc" + (r.id === id ? " sel" : "") + (m.art ? " art" : ""), style: artStyle(m),
      onclick: () => { ui.sel = r.id; ui.about = false; ui.records = false; ui.player = null; ui.target = null; renderList(); refreshDetail(true); renderMain({ top: true }); } },
      h("div", { class: "enc-title" }, !r.closed ? h("span", { class: "pill live" }, "Live") : null,
        r.imported ? h("span", { class: "pill", title: "Imported from a file someone shared" }, "Imported") : null,
        h("span", { style: "overflow:hidden;text-overflow:ellipsis" }, m.type || missionName(m)), diffTags(m)),
      h("div", { class: "enc-sub" }, h("span", { class: "enc-place" }, levelTag(m), h("span", null, missionPlace(r.mission))), h("span", null, times(r))),
      h("div", { class: "enc-sub dim" },
        h("span", null, d.toLocaleDateString(undefined, { month: "short", day: "numeric" }) + " " + d.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" })),
        // the experience the mission gave, where it can be compared down the list without opening each one; on a
        // line of its own size, so a long title with its difficulty tags never pushes it off the card
        h("span", null, r.xp ? h("span", { class: "enc-xp", title: "XP gained in this mission" }, `+${fmtFull(r.xp)} XP · `) : null, fmt(r.party_damage))));
  }));
}

// ---------- about ----------
function openAbout() { ui.about = true; ui.records = false; renderMain({ top: true }); }

// A link to somewhere outside the app. The app's window never leaves its own pages, so the app is asked to open
// the address in your default browser (by name: it only opens links from its own short list). In a normal
// browser tab, or if that fails, the link opens the usual way.
async function openOutside(name, url) {
  try {
    if (!(await act("/api/open/" + name)).ok) throw new Error("not opened");
  } catch (e) {
    window.open(url, "_blank", "noopener");
  }
}
function outsideLink(name, label) {
  const url = ((ui.live && ui.live.app && ui.live.app.links) || {})[name];
  if (!url) return h("span", { class: "path" }, label || name);
  return h("a", { class: "ext", href: url, title: url, onclick: e => { e.preventDefault(); openOutside(name, url); },
    onauxclick: e => e.preventDefault() }, label || url);
}

// What a user is entitled to know before deciding to use this: who and what wrote it, what it does to the game
// (nothing), where its knowledge of the game comes from, and what it keeps. The repository has the long form.
function aboutView() {
  const app = (ui.live && ui.live.app) || {};
  const card = (title, ...kids) => h("div", { class: "card about" }, h("h2", null, title), ...kids);
  const p = (...kids) => h("p", null, ...kids);
  const list = (...items) => h("ul", null, ...items.map(i => h("li", null, i)));
  const path = text => h("span", { class: "path" }, text);
  const repo = () => outsideLink("repo", app.repository || "the project's repository");
  return [
    h("div", { class: "head about-head" }, h("div", null, h("h1", null, "About zar-logs"),
      h("div", { class: "head-sub" }, "Version " + (app.version || "?") + " · unofficial fan tool, free and open source")),
      h("div", { class: "head-actions" }, h("button", { class: "btn", onclick: () => { ui.about = false; renderMain({ top: true }); } }, "Back"))),
    card("Entirely AI-programmed",
      p("This application and its development process were entirely AI-programmed. Anthropic's Claude, working through Claude Code, wrote "
        + "all of the code, did the reverse engineering of the game's data formats that the code relies on, and wrote the tests and the "
        + "documentation, this page included. The one exception is the disclosure at the top of the README, which the developer wrote."),
      p("The human developer (vicplusplus) chose what to build and the rules it must follow, played the game to test it, checked its numbers "
        + "against what the game shows, and reported what was wrong. They did not write the code and have not reviewed it in its entirety."),
      p("You have a right to know this before you decide whether to run it. The developer's own statement on it is the ",
        outsideLink("readme", "AI Disclosure"), " section of the README in ", repo(), ".")),
    card("What it does to the game: nothing",
      list("It runs as its own program and only reads the game's memory from outside. It asks Windows for read access and nothing more.",
        "It does not inject code, hook functions, patch or write memory, send input, or change any game or save file.",
        "It does not talk to the game's servers or to anything else on the network. The pages you see are served to this window from your own machine (127.0.0.1).",
        "It gives no advantage in play: it shows what happened (damage, healing, builds), after the game has computed it."),
      p(h("b", null, "Using this tool is against Ninja Kiwi's Terms of Service. "),
        "Their terms prohibit, without their written consent, capturing data from their games, in the same list as hacking, cheating and "
        + "botting. That this tool only reads and gives no advantage does not make it allowed. The terms say such use can get your account "
        + "segregated or suspended, with no compensation for lost progress or purchases. Use of this tool is entirely at your own risk. "
        + "Read them at ", outsideLink("terms"), ".")),
    card("Where its knowledge of the game comes from",
      p("Nothing about the game is built into this program. On first start, and after each game update, it reads your own installed copy: "
        + "the game's type metadata and code (for the layout of its data in memory), and its asset bundles (for names, values, texts and pictures)."),
      p("Working out how to do that took static analysis of the game's files: parsing its metadata, disassembling parts of its code, and "
        + "decoding its asset formats. Every method and tool, and what was done to the game during development, is listed in ",
        outsideLink("methods", "METHODS.md"), " in ", repo(), ".")),
    card("Ninja Kiwi's property",
      p("Zombie Assault: Resurgence and all of its assets, including the icons, pictures and texts shown here, are the property of Ninja Kiwi. "
        + "This project is not affiliated with or endorsed by Ninja Kiwi."),
      p("No game asset is distributed with this program. What you see is read from your own install and kept in a cache folder on your machine.")),
    card("Thanks and inspiration",
      p("The idea for this tool came from gbfr-logs, the damage meter for Granblue Fantasy: Relink, and from Cleista's fork of it in "
        + "particular. Many design choices and quality-of-life features here were taken from that site."),
      p("If you play Granblue Fantasy: Relink, have a look at it: it is a pretty cool tool. ",
        outsideLink("relink"), " · ", outsideLink("gbfr-logs"))),
    card("What it keeps on your machine",
      list("Recorded missions (logs). They hold the display names of the players in the mission, their loadouts and every hit. Account ids are not stored.",
        "A cache of what was read from the game, per game version.",
        "Nothing is uploaded. A log leaves your machine only when you send its file to someone yourself; the display names in it go with it.",
        "A log you import is someone else's file. It is checked before it is added, and builds in it are drawn from your own copy of the game, not from the file."),
      app.data ? p("Data folder: ", path(app.data)) : null),
    card("Problems and new versions",
      p("Found something wrong, or a number that does not match the game? Report it at ", outsideLink("issues"), "."),
      p("zar-logs does not check for updates. New versions are published at ", outsideLink("releases"), ".")),
  ];
}

// ---------- records ----------
async function openRecords() {
  ui.records = true; ui.about = false;
  renderList();
  renderMain({ top: true });
  await loadRecords();
}
async function loadRecords() {
  const q = ui.recordsVersion ? "?version=" + encodeURIComponent(ui.recordsVersion) : "";
  try { ui.recordsData = await getJSON("/api/records" + q); } catch (e) { ui.recordsData = { error: String(e) }; }
  if (ui.records) renderMain();
}
// A game version by what Steam calls it: its build number. A log from before logs named their version gets the
// version that was installed when it was played, which is a best guess.
function buildName(g) {
  if (!g || !g.build) return g && g.id ? "game version " + g.id.slice(0, 6) : "unknown game version";
  return "build " + g.build;
}
function versionPicker(r) {
  const vs = r.versions || [];
  if (!vs.length) return null;
  const opts = vs.map(v => h("option", { value: v.id, selected: r.version === v.id ? "" : null },
    `${buildName(v)}${v.current ? " (current)" : ""}`));
  opts.push(h("option", { value: "all", selected: r.version === "all" ? "" : null }, "All game versions"));
  return h("select", { class: "pick", "aria-label": "Game version", title: "Records are kept per game version, so a balance patch starts them afresh",
    onchange: e => { ui.recordsVersion = e.target.value; loadRecords(); } }, opts);
}
function openLog(id) {
  ui.sel = id; ui.records = false; ui.about = false; ui.player = null; ui.target = null;
  renderList(); refreshDetail(true); renderMain({ top: true });
}
function recordValue(unit, v) {
  if (unit === "time") return timer(v);
  if (unit === "xp") return fmtFull(v);
  if (unit === "xp_min") return fmtFull(v) + " / min";
  return fmt(v);
}
function recordHead(unit) { return { dps: "DPS", xp: "XP", xp_min: "XP per minute", time: "Mission time" }[unit] || ""; }
// the records, as tabs: [key, tab label, what it is]
const RECORD_TABS = [
  ["team_dps", "Team DPS", "The party's damage over the mission time."],
  ["team_boss", "Team DPS on a boss", "The damage the party dealt to a boss over the time from the first hit to its end, as on the boss card. The fight's length is shown with it."],
  ["own_dps", "Personal DPS", "Your character's damage over the mission time."],
  ["own_boss", "Personal DPS on a boss", "Your character's damage to a boss over the time from the first hit to its end. The fight's length is shown with it."],
  ["xp", "XP", "The XP a mission gave, from the result screen."],
  ["xp_min", "XP per minute", "The XP a mission gave over its mission time."],
  ["clear", "Fastest clear", "The mission time of a mission that was won. Per mission only: across missions it says nothing."],
];
// One record's table: every character's best, best first. A row opens the log it comes from.
function recordTable(rec, opts) {
  const rows = rec.rows || [];
  if (!rows.length) return h("div", { class: "note rec-empty" }, rec.key === "clear" ? "No mission won yet." : "Nothing recorded yet.");
  const body = rows.map((r, i) => {
    const m = r.mission || {};
    const d = new Date(r.start * 1000);
    const where = opts.place ? (m.story ? m.name : [m.type, m.name].filter(Boolean).join(" · ")) : m.name;
    const bits = [rec.unit === "time" ? null : timer(r.duration), r.party === 1 ? "solo" : `${r.party} players`, r.won ? null : "not cleared"];
    return h("tr", { class: "click", tabindex: 0, title: "Open this mission's log", onclick: () => openLog(r.id),
        onkeydown: e => { if (e.key === "Enter") openLog(r.id); } },
      h("td", { class: "rank" }, String(i + 1)),
      h("td", { class: "wide" }, h("div", { class: "namecell" }, h("span", { class: "name" }, r.name)),
        h("div", { class: "rec-sub" }, [r.cls, r.sub].filter(Boolean).join(" · "))),
      h("td", null, h("b", { title: rec.unit === "dps" ? fmtFull(r.value) + " damage per second" : null }, recordValue(rec.unit, r.value)),
        r.fight != null ? h("div", { class: "rec-sub", title: `${r.boss || "Boss"}: from the first hit to its end` }, `${timer(r.fight)} fight`) : null),
      h("td", { class: "left rec-mission" }, h("div", { class: "rec-tags" }, where ? h("span", null, where) : null, diffTags(m), levelTag(m)),
        h("div", { class: "rec-sub" }, [r.boss, ...bits].filter(Boolean).join(" · "))),
      h("td", { class: "rec-sub" }, d.toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" }),
        opts.all && r.build ? h("div", null, "build " + r.build) : null));
  });
  return simpleTable(["#", "Character", recordHead(rec.unit), "Mission", "Date"], body);
}
// a section of the page: a banner with the mission's picture, then the chosen record's table
function recordSection(title, sub, art, rec, opts) {
  return h("div", { class: "card rec" },
    h("div", { class: "rec-banner" + (art ? " art" : ""), style: art ? `background-image:url("${art}")` : null },
      h("div", { class: "rec-banner-title" }, title), sub ? h("div", { class: "rec-banner-sub" }, sub) : null),
    h("div", { class: "rec-body" }, rec ? recordTable(rec, opts) : h("div", { class: "note rec-empty" }, "Nothing recorded yet.")));
}
function recordsView() {
  const r = ui.recordsData;
  const back = h("div", { class: "head-actions" }, r && !r.error ? versionPicker(r) : null,
    h("button", { class: "btn", onclick: () => { ui.records = false; renderList(); renderMain({ top: true }); } }, "Back"));
  const head = sub => h("div", { class: "head about-head" }, h("div", null, h("h1", null, "Records"), h("div", { class: "head-sub" }, sub)), back);
  if (!r) return [head("Reading the logs…")];
  if (r.error) return [head("The records could not be read: " + r.error)];
  const chars = r.characters || [];
  const ver = (r.versions || []).find(v => v.id === r.version);
  const onWhat = r.version === "all" ? "on every game version" : ver ? `on ${buildName(ver)}` : "";
  const tab = RECORD_TABS.find(t => t[0] === ui.recordsTab) || RECORD_TABS[0];
  const opts = { all: r.version === "all" };
  const pick = list => (list || []).find(x => x.key === tab[0]);
  const out = [head(`${r.missions_counted} ${r.missions_counted === 1 ? "mission" : "missions"} of your own `
      + `${chars.length === 1 ? "character" : "characters"}${chars.length ? " (" + chars.join(", ") + ")" : ""} ${onWhat}`
      + (ver && ver.inferred ? " (worked out from the dates: these logs are from before logs named their game version)" : "")),
    h("div", { class: "tabs rec-tabs", role: "tablist" }, RECORD_TABS.map(([k, label]) =>
      h("button", { class: k === tab[0] ? "on" : "", role: "tab", "aria-selected": k === tab[0],
        onclick: () => { ui.recordsTab = k; renderMain(); } }, label))),
    h("div", { class: "note rec-note" }, tab[2] + " One row per character, its best; only the player marked as you in each log counts. "
      + "Records are kept per game version, so a balance patch starts them afresh. Click a row to open that mission.")];
  const global = pick(r.global);
  if (global) out.push(recordSection("All missions", `${r.missions_counted} ${r.missions_counted === 1 ? "mission" : "missions"}`, null, global,
    Object.assign({ place: true }, opts)));
  for (const g of r.missions || []) {
    out.push(recordSection(g.title || "Mission", (g.story ? "Story mission · " : "Operation · ") + `${g.count} ${g.count === 1 ? "mission" : "missions"}`,
      g.art, pick(g.records), Object.assign({ place: false }, opts)));
  }
  if (r.skipped) out.push(h("div", { class: "note" }, `${r.skipped} log file(s) could not be read and are left out.`));
  return out;
}

// ---------- main ----------
function tile(label, value, note) {
  return h("div", { class: "tile" }, h("div", { class: "tile-label" }, label), h("div", { class: "tile-value" }, value),
    note ? h("div", { class: "tile-note" }, note) : null);
}
function playerColor(d, cid) {
  const order = d.players.map(p => p.cid).sort((a, b) => a - b);
  return SERIES[order.indexOf(cid) % SERIES.length];
}
function seg(options, value, onpick) {
  return h("div", { class: "seg", role: "group" }, options.map(([v, label]) =>
    h("button", { class: v === value ? "on" : "", "aria-pressed": v === value, onclick: () => onpick(v) }, label)));
}

// Re-rendering replaces the page content. Without care the document briefly collapses to nothing and the browser
// clamps the scroll position to the top, which on a live log (redrawn every 2 s) throws the reader back up.
// So: hold the page height while the content is swapped, then put the scroll position back.
function renderMain(opts) {
  const main = document.getElementById("main");
  const keep = !(opts && opts.top);
  const x = window.scrollX, y = window.scrollY;
  const inner = keep ? [...main.querySelectorAll(".scroll-x")].map(e => e.scrollLeft) : [];
  if (keep) main.style.minHeight = main.offsetHeight + "px";
  try {
    renderMainInner(main);
  } finally {
    if (keep) {
      main.querySelectorAll(".scroll-x").forEach((e, i) => { if (inner[i]) e.scrollLeft = inner[i]; });
      main.style.minHeight = "";
      if (window.scrollY !== y || window.scrollX !== x) window.scrollTo(x, y);
    } else {
      window.scrollTo(0, 0);
    }
    ui.stale = false;
  }
}

function renderMainInner(main) {
  const d = ui.detail;
  Tip.hide();
  window.chartHover = window.cardHover = false;   // every hover target is about to be replaced
  if (ui.about) {
    main.replaceChildren(...aboutView());
    return;
  }
  if (ui.records) {
    main.replaceChildren(...recordsView());
    return;
  }
  if (!d) {
    main.replaceChildren(ui.notice ? h("div", { class: "notice", role: "status" }, ui.notice) : "", h("div", { class: "head" }, h("div", null, h("h1", null, "zar-logs"),
      h("div", { class: "head-sub" }, "Waiting for a mission. Encounters are recorded automatically while the meter is running."))),
      h("div", { class: "note" }, "This program was entirely programmed by an AI system and is an unofficial, read-only fan tool. ",
        h("button", { class: "btn small", onclick: openAbout }, "About, AI use and methods")));
    return;
  }
  if (ui.player == null || !d.players.some(p => p.cid === ui.player)) {
    const me = d.players.find(p => p.local) || d.players[0];
    ui.player = me ? me.cid : null;
  }
  const when = new Date(d.start * 1000);
  const m = d.mission || {};
  const subBits = [missionPlace(m), when.toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" }),
    d.game ? buildName(d.game) + (d.game.inferred ? " (the one installed then)" : "") : null].filter(Boolean);
  const head = h("div", { class: "head" + (m.art ? " art" : ""), style: artStyle(m) },
    h("div", null, h("h1", null, (m.type || missionName(m)) + (d.duration ? ` | ${times(d)}` : ""), diffTags(m)),
      h("div", { class: "head-sub" }, levelTag(m), subBits.join(" · "))),
    h("div", { class: "head-actions" },
      !d.closed ? h("span", { class: "pill live" }, "Live") : null,
      d.imported ? h("span", { class: "pill", title: "Imported from a file someone shared" }, "Imported") : null,
      d.closed ? h("button", { class: "btn", title: "Show this mission's log file in Explorer, to drag it to wherever you want to send it", onclick: () => showInFolder(d.id) }, "Show in folder") : null,
      d.closed ? h("button", { class: "btn danger", onclick: () => removeEncounter(d.id) }, "Delete") : null));

  // what the figures count besides the players' own damage to enemies: structures (crates, pods, barrels, level
  // geometry) and the blasts of enemies that explode. A log from before these were kept apart has them in.
  const extras = [d.structures === "on" ? "structures" : "", d.blasts === "on" ? "enemy explosions" : ""].filter(Boolean);
  const counted = d.structures === "always" ? "structures and enemy explosions included" : extras.length ? "with " + extras.join(" and ") : "";
  const tiles = h("div", { class: "tiles" },
    tile("Party damage", fmt(d.party_damage), [fmtFull(d.party_damage), counted].filter(Boolean).join(" · ")),
    tile("Party DPS", fmt(d.party_dps), "damage / mission time"),
    tile("Mission time", timer(d.duration), [
      !d.closed && !d.game_time && d.result == null ? "running" :
        d.outcome === "won" ? "mission complete" : d.outcome === "lost" ? "mission failed" :
        d.result === "finished" ? "mission ended" : "left before the end",
      d.game_time ? `game timer ${timer(d.game_time)} with cutscenes` : null].filter(Boolean).join(" · ")),
    tile("Kills", fmtFull(d.players.reduce((a, p) => a + (p.kills_native || p.kills), 0))),
    d.xp ? tile("XP gained", fmtFull(d.xp), "from the result screen") : null);

  if (ui.target != null && !(d.bosses || []).some(b => b.id === ui.target)) ui.target = null;
  main.replaceChildren(ui.notice ? h("div", { class: "notice", role: "status" }, ui.notice) : "", head, tiles, partyCard(d), timeCard(d), playerCard(d), captureNote(d),
    h("div", { class: "note legal" }, "Unofficial fan tool, free to use. Zombie Assault: Resurgence and all of its assets, including the icons shown here, are the property of Ninja Kiwi. "
      + "This tool is not affiliated with or endorsed by Ninja Kiwi. Icons are read from your own installed copy of the game and are not distributed with the tool."));
  drawCharts(d);
}

function partyCard(d) {
  const top = Math.max(...d.players.map(p => p.damage), 1);
  const rows = d.players.map(p => {
    const color = playerColor(d, p.cid);
    return h("tr", { class: "click" + (p.cid === ui.player ? " sel" : ""), tabindex: 0,
      onclick: () => { ui.player = p.cid; renderMain(); },
      onkeydown: e => { if (e.key === "Enter") { ui.player = p.cid; renderMain(); } } },
      h("td", { class: "wide" }, h("div", { class: "namecell" }, h("i", { class: "swatch", style: `background:${color}` }),
        h("span", { class: "name" }, p.name), h("span", { class: "sub" }, [p.cls, p.sub].filter(Boolean).join(" · ") + (p.lvl ? ` · L${p.lvl}` : "")),
        p.local ? h("span", { class: "tag" }, "you") : null),
        h("div", { class: "meter" }, h("i", { style: `width:${(p.damage / top * 100).toFixed(1)}%;background:${color}` }))),
      h("td", null, fmt(p.damage)), h("td", null, fmt(p.dps)), h("td", null, pct(p.share)),
      h("td", null, pct(critRate(p))), h("td", null, fmt(p.max)), h("td", null, fmtFull(p.kills_native || p.kills)),
      h("td", null, fmt(p.taken)), h("td", null, fmt(p.healed)), h("td", null, String(p.downs)));
  });
  // Two kinds of hit are in every log next to the players' own damage to enemies, and each can be counted or
  // not, here and everywhere else (the list, the overlay): damage to structures, and the blasts of enemies that
  // explode, which the game credits to a player. A log from before they were kept apart has only the sum; one of
  // version 0.2.0 has structures apart and no blasts.
  const old = d.structures === "always";
  const toggle = (label, title, state, what) => state === "on" || state === "off"
    ? h("button", { class: "check" + (state === "on" ? " on" : ""), "aria-pressed": String(state === "on"), title, onclick: () => setCounted(what, state !== "on") },
      h("i", { "aria-hidden": "true" }, state === "on" ? "✓" : ""), label) : null;
  const pick = old ? null : h("div", { class: "checks" }, h("span", null, "Also count"),
    toggle("Structures", "Damage to crates, pods, barrels and level geometry", d.structures, "structures"),
    toggle("Enemy explosions", "Blasts of enemies that explode (Detonators), which the game credits to a player", d.blasts, "blasts"));
  const note = old ? " · recorded by version 0.1: damage to structures and enemy explosions are in these figures"
    : d.blasts === "never" ? " · recorded by version 0.2.0: enemy explosions were not recorded" : "";
  return h("section", { class: "card" },
    h("div", { class: "card-head" }, h("div", null, h("div", { class: "card-title" }, "Party"),
      h("div", { class: "card-sub" }, "Select a player to see their breakdown below" + note)),
      h("div", { class: "controls" }, pick)),
    h("div", { class: "scroll-x" }, h("table", null,
      h("thead", null, h("tr", null, ["Player", "Damage", "DPS", "Share", "Crit rate", "Max hit", "Kills", "Taken", "Healed", "Downs"].map(t => h("th", t === "Crit rate" ? { title: "Crits out of the hits of the player's weapons. Turrets, abilities and other sources do not crit and are left out." } : null, t)))),
      h("tbody", null, rows))));
}

function timeCard(d) {
  const p = d.players.find(x => x.cid === ui.player);
  const boss = (d.bosses || []).find(b => b.id === ui.target);
  const what = boss ? `Damage to ${boss.name}` : "Damage";
  const pick = (d.bosses || []).length ? h("select", { class: "pick", "aria-label": "Target shown in the chart",
    onchange: e => { ui.target = e.target.value === "" ? null : Number(e.target.value); renderMain(); } },
    h("option", { value: "" }, "All targets"),
    d.bosses.map(b => h("option", Object.assign({ value: String(b.id) }, b.id === ui.target ? { selected: "selected" } : {}), b.name))) : null;
  return h("section", { class: "card" },
    h("div", { class: "card-head" },
      h("div", null, h("div", { class: "card-title" }, ui.mode === "dps" ? `${what} per second over time` : `Cumulative ${what.toLowerCase()} over time`)),
      h("div", { class: "controls" }, pick,
        ui.mode === "dps" ? seg([[1, "1s"], [3, "3s"], [5, "5s"]], ui.window, v => { ui.window = v; renderMain(); }) : null,
        seg([["dps", "DPS"], ["cumulative", "Cumulative"]], ui.mode, v => { ui.mode = v; renderMain(); }))),
    boss ? bossStrip(d, boss) : null,
    d.players.length > 1 ? legend(d.players.map(pl => ({ name: pl.name, color: playerColor(d, pl.cid) })), true) : null,
    h("div", { class: "chart", id: "chart-time" }));
}

// what is known about the selected boss: a snapshot row above its chart
function bossStrip(d, b) {
  const alive = Math.max(0, b.end - b.start);
  const fight = b.first_hit != null ? Math.max(0, b.end - b.first_hit) : alive;
  const bits = [
    ["Max health", b.hp_max ? fmt(b.hp_max) : "–"],
    ["Damage dealt to it", fmt(b.damage)],
    ["Appeared at", timer(b.start)],
    [b.killed ? "Killed at" : "Last seen at", timer(b.end)],
    [b.killed ? "Time to kill" : "Time alive", timer(b.killed ? fight : alive)],
    // over the time it was being hit: a boss that dies in under a second is not averaged over a whole one
    ["DPS on it", fight > 0.01 ? fmt(b.damage / fight) : "–"],
  ];
  if (b.mods && b.mods.length) bits.push(["Modifiers", b.mods.map(x => x.replace("{0}", "").trim()).join(", ")]);
  const df = b.defense || {};
  return h("div", { class: "boss" },
    b.icon ? h("div", { class: "boss-pic" }, icon(b.icon, "", null)) : null,
    h("div", { class: "boss-body" },
      h("div", { class: "strip" }, bits.map(([k, v]) => h("div", null, h("span", null, k), h("b", null, v)))),
      h("div", { class: "strip def" }, defenseBits(b, df, "the party's", { brief: !!b.hp_max }))));
}

// ---- what a target can take: health, resistances per damage type, status immunities, and how much they removed ----
function span(lo, hi, f) { return Math.abs(hi - lo) < 1e-6 ? f(lo) : `${f(lo)} – ${f(hi)}`; }
function resText(lo, hi) {
  const one = v => v >= 0.999 ? "Immune" : v < 0 ? `takes +${Math.round(-v * 100)}%` : `${Math.round(v * 100)}%`;
  return span(lo, hi, one);
}
function resChips(rows) {
  return h("div", { class: "chips" }, rows.map(([name, lo, hi]) =>
    h("span", { class: "dtype chip" }, h("i", { style: `background:${DTYPE_COLOR[name] || "var(--muted)"}` }), name, h("b", null, resText(lo, hi)))));
}
function mitigationText(x, whose) {
  if (x.mitigated == null) return null;
  const v = x.mitigated, est = x.mitigated_est ? "about " : "";
  if (Math.abs(v) < 0.0005) return `None: ${whose} damage landed in full`;
  return v > 0 ? `${est}${pct(v)} of ${whose} damage resisted` : `${est}${pct(-v)} extra from weaknesses`;
}
// the cells of a defence row: [label, node] pairs wrapped the way .strip expects
// brief: for the boss strip, which has the maximum health beside it and the resistances to read the rest from
function defenseBits(x, df, whose, opts) {
  const brief = !!(opts && opts.brief);
  const cell = (k, v) => h("div", null, h("span", null, k), typeof v === "string" ? h("b", null, v) : v);
  const out = [];
  if (!df.source) {
    out.push(cell("Defences", "not recorded for this target"));
  } else {
    if (df.hp && !brief) out.push(cell(df.source === "base" ? "Base health" : "Health", span(df.hp[0], df.hp[1], fmtFull)));
    if (df.shield && df.shield[1] > 0) out.push(cell("Shield", span(df.shield[0], df.shield[1], fmtFull)));
    out.push(cell("Resistances", df.resist && df.resist.length ? resChips(df.resist) : "none"));
    if (df.shield_resist && df.shield_resist.length) out.push(cell("Shield resistances", resChips(df.shield_resist)));
    if (df.explosion) out.push(cell("Explosion resistance", span(df.explosion[0], df.explosion[1], v => pct(v, 0))));
    if (df.dodge) out.push(cell("Dodge chance", span(df.dodge[0], df.dodge[1], v => pct(v, 0))));
    if (df.regen) out.push(cell("Health regen", span(df.regen[0], df.regen[1], v => pct(v, 1)) + " /s"));
    if (df.elemental) out.push(cell("Elemental effects on it", span(df.elemental[0], df.elemental[1], v => pct(v, 0)) + " shorter"));
    if (df.immune && df.immune.length) out.push(cell("Immune to", df.immune.join(", ")));
    for (const [name, lo, hi] of df.weaker || []) out.push(cell(`${name} strength`, "−" + span(lo, hi, v => pct(v, 0))));
    if (df.elite) out.push(cell("Elite variants", (df.mods || []).join(", ") || String(df.elite)));
  }
  for (const [name, pool, lo, hi] of df.observed || [])
    out.push(cell(`${name} applied on hits${pool === "shield" ? " (shield)" : ""}`, resText(lo, hi)));
  const mit = brief ? null : mitigationText(x, whose);
  if (mit) out.push(cell("Mitigation", mit));
  return out;
}
function defenseCard(x, whose) {
  const df = x.defense || {};
  return h("div", { class: "tcard def" },
    h("div", { class: "tcard-title" }, h("span", null, x.name), h("b", null, targetKind(x)[0])),
    h("div", { class: "def-grid" }, defenseBits(x, df, whose)),
    df.source === "base" ? h("div", { class: "tcard-note" }, "Base values from the game data: this log was recorded before the meter read them from the mission, so difficulty scaling and elite modifiers are missing.") : null,
    df.variants > 1 ? h("div", { class: "tcard-note" }, `${df.variants} variants of this enemy were hit (stages, elites); ranges cover all of them.`) : null);
}

function rolling(values, w) {
  const out = new Array(values.length);
  let acc = 0;
  for (let i = 0; i < values.length; i++) {
    acc += values[i];
    if (i >= w) acc -= values[i - w];
    out[i] = acc / Math.min(w, i + 1);
  }
  return out;
}
function cumulative(values) { let a = 0; return values.map(v => (a += v)); }
function downsample(values, maxPts, how) {
  const k = Math.ceil(values.length / maxPts);
  if (k <= 1) return [values, 1];
  const out = [];
  for (let i = 0; i < values.length; i += k) {
    const sl = values.slice(i, i + k);
    out.push(how === "last" ? sl[sl.length - 1] : sl.reduce((a, b) => a + b, 0) / sl.length);
  }
  return [out, k];
}

function sourceSlots(d, p) {
  const byEnc = ui.srcColors[d.id] || (ui.srcColors[d.id] = {});
  const map = byEnc[p.cid] || (byEnc[p.cid] = {});
  for (const src of p.sources) {
    if (!(src.key in map) && Object.keys(map).length < 5) map[src.key] = Object.keys(map).length;
  }
  return map;
}
function sourceColor(map, key) { return key in map ? SERIES[map[key]] : OTHER; }

function playerCard(d) {
  const p = d.players.find(x => x.cid === ui.player);
  if (!p) return h("div");
  const tabs = [["sources", "Damage by source"], ["targets", "Targets"], ["taken", "Damage taken"], ["effects", "Abilities and effects"], ["build", "Build"]];
  if (!tabs.some(t => t[0] === ui.tab)) ui.tab = "sources";
  const body = { sources: sourcesTab, targets: targetsTab, taken: takenTab, effects: effectsTab, build: buildTab }[ui.tab](d, p);
  return h("section", { class: "card" },
    h("div", { class: "card-head" }, h("div", null,
      h("div", { class: "card-title" }, h("span", { class: "namecell" }, h("i", { class: "swatch", style: `background:${playerColor(d, p.cid)}` }), p.name)),
      h("div", { class: "card-sub" }, `${fmt(p.damage)} damage · ${fmt(p.dps)} DPS · ${fmtFull(p.hits)} hits · ${pct(critRate(p))} weapon crit rate`))),
    h("div", { class: "tabs", role: "tablist" }, tabs.map(([k, label]) =>
      h("button", { class: k === ui.tab ? "on" : "", role: "tab", "aria-selected": k === ui.tab, onclick: () => { ui.tab = k; renderMain(); } }, label))),
    body);
}

// what a damage source is. "enemy" = an enemy that explodes (Detonators): the game credits the blast to a player, and
// it is only among the sources when "Enemy explosions" is counted (or in a log of version 0.1, where it always is)
function kindLabel(k) {
  return { weapon: "Weapon", ability: "Ability", effect: "Ability effect", enemy: "Enemy explosion", turret: "Turret", barrel: "Environment",
    area: "Area effect", entity: "Entity", behaviour: "Effect", shield: "Shield", environment: "Environment" }[k] || k || "";
}

function sourcesTab(d, p) {
  if (!p.sources.length) return h("div", { class: "empty" }, "No damage recorded for this player.");
  const map = sourceSlots(d, p);
  const colored = p.sources.filter(x => x.key in map).sort((a, b) => map[a.key] - map[b.key]);
  const rest = p.sources.filter(x => !(x.key in map));
  const parts = colored.map(x => ({ name: x.name, value: x.damage, color: SERIES[map[x.key]], darkInk: map[x.key] === 3 }));
  const restSum = rest.reduce((a, x) => a + x.damage, 0);
  if (restSum > 0) parts.push({ name: `Other (${rest.length})`, value: restSum, color: OTHER });
  const top = p.sources[0].damage || 1;
  const ct = Math.max(d.duration, 1);
  // a power weapon (picked up from a supply crate) is named in its red, with its card on hover
  const srcName = x => {
    const cell = h("div", { class: "namecell" + (x.power ? " hov power" : "") }, h("i", { class: "swatch", style: `background:${sourceColor(map, x.key)}` }),
      h("span", { class: "name" }, x.name), h("span", { class: "sub" }, x.power ? "Power weapon" : kindLabel(x.kind)));
    return x.power ? hoverCard(cell, () => powerCard(x.power)) : cell;
  };
  const rows = p.sources.map(x => h("tr", null,
    h("td", { class: "wide" }, srcName(x),
      h("div", { class: "meter" }, h("i", { style: `width:${(x.damage / top * 100).toFixed(1)}%;background:${sourceColor(map, x.key)}` }))),
    h("td", null, fmt(x.damage)), h("td", null, pct(p.damage ? x.damage / p.damage : 0)), h("td", null, fmt(x.damage / ct)),
    h("td", null, fmtFull(x.hits)), h("td", null, fmt(x.hits ? x.damage / x.hits : 0)), h("td", null, fmt(x.min)), h("td", null, fmt(x.max)),
    h("td", null, pct(x.hits ? x.crits / x.hits : 0)), h("td", null, pct(x.damage ? x.dot / x.damage : 0, 0)),
    h("td", null, fmtFull(x.kills)), h("td", null, fmt(x.overkill))));
  return h("div", null,
    segmentedBar(parts), legend(parts.map(x => ({ name: x.name, color: x.color }))),
    h("div", { class: "scroll-x" }, h("table", null,
      h("thead", null, h("tr", null, ["Source", "Damage", "Share", "DPS", "Hits", "Avg", "Min", "Max", "Crit rate", "DoT", "Kills", "Overkill"].map(t => h("th", null, t)))),
      h("tbody", null, rows))),
    oldKillsNote(d, p, p.sources),
    h("div", { class: "card-head", style: "margin-top:20px" }, h("div", null, h("div", { class: "card-title" }, "Sources over time"),
      h("div", { class: "card-sub", id: "src-sub" }, "Damage per second by source"))),
    h("div", { class: "chart", id: "chart-src" }));
}

function simpleTable(headers, rows) {
  return h("div", { class: "scroll-x" }, h("table", null, h("thead", null, h("tr", null, headers.map(t => h("th", null, t)))), h("tbody", null, rows)));
}
function barCell(name, sub, share, color) {
  return h("td", { class: "wide" }, h("div", { class: "namecell" }, h("span", { class: "name" }, name), sub ? h("span", { class: "sub" }, sub) : null),
    h("div", { class: "meter" }, h("i", { style: `width:${(share * 100).toFixed(1)}%;background:${color}` })));
}
// targets are coloured by what they are: boss, enemy, or a prop of the level
function targetKind(x) {
  if (x.boss) return ["Boss", "var(--series-8)"];
  if (x.type === "Enemy") return ["Enemy", "var(--series-1)"];
  if (x.type === "Explosive barrel") return ["Barrel", "var(--series-4)"];
  return [x.type || "Other", "var(--other)"];
}
function targetsTab(d, p) {
  if (!p.targets.length) return h("div", { class: "empty" }, "No targets recorded.");
  const top = p.targets[0].damage || 1;
  const kinds = [];
  for (const x of p.targets) { const [k, c] = targetKind(x); if (!kinds.some(y => y.name === k)) kinds.push({ name: k, color: c }); }
  return h("div", null, legend(kinds),
    simpleTable(["Target", "Damage", "Share", "Hits", "Avg", "Max", "Crit rate", "Resisted", "Kills"], p.targets.map(x => {
      const [kind, color] = targetKind(x);
      const whose = p.local ? "your" : `${p.name}'s`;
      const name = hoverCard(h("div", { class: "namecell hov" }, h("i", { class: "swatch", style: `background:${color}` }),
        x.icon ? h("span", { class: "tgt-pic" }, icon(x.icon, "", null)) : null,
        h("span", { class: "name" }, x.name), h("span", { class: "sub" }, kind)), () => defenseCard(x, whose));
      const res = (x.defense || {}).resist || [];
      return h("tr", null,
        h("td", { class: "wide" }, name,
          h("div", { class: "meter" }, h("i", { style: `width:${(x.damage / top * 100).toFixed(1)}%;background:${color}` }))),
        h("td", null, fmt(x.damage)), h("td", null, pct(p.damage ? x.damage / p.damage : 0)), h("td", null, fmtFull(x.hits)),
        h("td", null, fmt(x.hits ? x.damage / x.hits : 0)), h("td", null, fmt(x.max)), h("td", null, pct(critRate(x))),
        h("td", null, h("span", { class: "resisted" }, res.map(([n]) => h("i", { title: n, style: `background:${DTYPE_COLOR[n] || "var(--muted)"}` })),
          x.mitigated == null ? "–" : (x.mitigated_est ? "≈" : "") + pct(x.mitigated))),
        h("td", null, fmtFull(x.kills)));
    })),
    oldKillsNote(d, p, p.targets),
    h("div", { class: "note" }, "Hover a target for its health, resistances and status immunities. “Resisted” is the share of the damage aimed at it that its resistances removed, from the resistance the game applied on each hit."));
}
function takenTab(d, p) {
  // Logs of earlier versions list hits that did nothing (a dash "hits" its own user for 0), and an attacker once
  // per difficulty stage it came in. Neither belongs here: rows without damage are left out, and rows of one name
  // and kind are added up.
  const byName = new Map();
  for (const x of p.taken_by) {
    if (!(x.damage > 0)) continue;
    const k = x.name + "\u0000" + (x.kind || "");
    const row = byName.get(k);
    if (row) { row.damage += x.damage; row.hits += x.hits; } else byName.set(k, Object.assign({}, x));
  }
  const rows = [...byName.values()].sort((a, b) => b.damage - a.damage);
  const top = (rows[0] && rows[0].damage) || 1;
  return h("div", null,
    h("div", { class: "card-sub", style: "margin-bottom:8px" }, `${fmt(p.taken)} damage taken in ${fmtFull(rows.reduce((a, x) => a + x.hits, 0))} hits · ${p.downs} downs · ${fmt(p.healed)} healed`),
    simpleTable(["From", "Damage", "Share", "Hits", "Avg"], rows.map(x => h("tr", null,
      barCell(x.name, "", x.damage / top, "var(--series-2)"),
      h("td", null, fmt(x.damage)), h("td", null, pct(p.taken ? x.damage / p.taken : 0)), h("td", null, fmtFull(x.hits)),
      h("td", null, fmt(x.hits ? x.damage / x.hits : 0))))),
    h("div", { class: "card-head", style: "margin-top:20px" }, h("div", null, h("div", { class: "card-title" }, "Health over time"),
      h("div", { class: "card-sub" }, "Health and shield, sampled each second"))),
    legend([{ name: "Health", color: "var(--series-3)" }, { name: "Shield", color: "var(--series-1)" }], true),
    h("div", { class: "chart", id: "chart-hp" }));
}
// ---------- abilities and effects table (the timeline itself sits under the damage chart) ----------
function effectsTab(d, p) {
  const lanes = playerRows(d, p);
  if (!lanes.length) return h("div", { class: "empty" }, "No abilities or status effects recorded for this player.");
  const rows = [];
  let group = null;
  for (const ln of lanes) {
    if (ln.group !== group) {
      group = ln.group;
      rows.push(h("tr", { class: "group" }, h("td", { colspan: 5 }, ln.groupLabel || group)));
    }
    const untimed = ln.kind === "ability" && ln.timed === false;       // an instant ability has no uptime
    rows.push(h("tr", null,
      h("td", null, h("div", { class: "namecell" }, ln.indent ? null : h("i", { class: "swatch", style: `background:${ln.color}` }),
        h("span", { class: ln.indent ? "name indent" : "name" }, ln.name))),
      h("td", { class: "left" }, ln.kind === "ability" ? "Ability" : ln.source || ""),
      h("td", null, untimed ? "–" : pct(ln.uptime)), h("td", null, untimed ? "–" : timer(ln.total || 0)),
      h("td", null, String(ln.applications))));
  }
  return simpleTable(["Ability / effect", "Source", "Uptime", "Active time", "Uses"], rows);
}

// ---------- build, laid out like the game's loadout screens ----------
const QVAR = ["var(--q0)", "var(--q1)", "var(--q2)", "var(--q3)", "var(--q4)"];
const DTYPE_COLOR = { Physical: "var(--dt-physical)", Thermal: "var(--dt-thermal)", Cryo: "var(--dt-cryo)", Energy: "var(--dt-energy)",
  Corrosion: "var(--dt-corrosion)", Void: "var(--dt-void)" };
const GLYPH = {
  Optic: "M12 4a8 8 0 1 0 0 16a8 8 0 0 0 0-16zM12 1v6M12 17v6M1 12h6M17 12h6",
  Barrel: "M12 3a9 9 0 1 0 0 18a9 9 0 0 0 0-18zM12 8a4 4 0 1 0 0 8a4 4 0 0 0 0-8z",
  Grip: "M7 4h10l-1.5 6H14l-1 10H9L8 10H7z",
  Core: "M12 2l8.5 5v10L12 22l-8.5-5V7z",
  Clip: "M7 3h10v18H7zM10 8h4M10 12h4M10 16h4",
  Armor: "M12 2l8 3v6c0 5-3.4 9-8 11c-4.6-2-8-6-8-11V5z",
};
GLYPH.Armour = GLYPH.Armor;   // the game's own spelling of the armour mod slot
function glyph(name) {
  return s("svg", { viewBox: "0 0 24 24", width: 20, height: 20, fill: "none", stroke: "currentColor", "stroke-width": 1.6,
    "stroke-linejoin": "round", "stroke-linecap": "round", "aria-hidden": "true" }, s("path", { d: GLYPH[name] || GLYPH.Core }));
}
// an in-game icon read from the local install; when it is missing the fallback node is shown instead
function icon(url, cls, fallback) {
  // pictures only ever come from this app's own picture cache, whatever a log says
  if (!url || !url.startsWith("/icons/")) return fallback || null;
  const img = h("img", { src: url, class: cls || "", alt: "", loading: "lazy", draggable: "false" });
  img.addEventListener("error", () => { if (fallback) img.replaceWith(fallback); else img.remove(); });
  return img;
}
function pips(n, max) {
  return h("div", { class: "pips", "aria-label": `${n} of ${max}` }, Array.from({ length: max }, (_, i) => h("i", { class: i < n ? "on" : "" })));
}
function hoverCard(el, make) {
  el.addEventListener("pointermove", e => { window.cardHover = true; Tip.card(e, make()); });
  el.addEventListener("pointerleave", () => { window.cardHover = false; Tip.hide(); if (ui.stale) renderMain(); });
  el.addEventListener("focus", () => { const r = el.getBoundingClientRect(); Tip.card({ clientX: r.right, clientY: r.top }, make()); });
  el.addEventListener("blur", () => Tip.hide());
  el.tabIndex = 0;
  return el;
}
function statList(stats) {
  return h("div", { class: "gear-stats" }, stats.map(([k, v]) => [h("span", null, k), h("b", null, v)]));
}
function dtypeChip(name) {
  return name ? h("span", { class: "dtype" }, h("i", { style: `background:${DTYPE_COLOR[name] || "var(--muted)"}` }), name) : null;
}
function modCard(md) {
  return h("div", { class: "tcard" },
    h("div", { class: "tcard-bar", style: `background:${QVAR[md.quality] || QVAR[0]}` }, md.qname),
    h("div", { class: "tcard-title" }, h("span", null, md.slot_label || `${md.slot} mod`), md.power != null ? h("b", null, `${md.power} power`) : null),
    md.stats && md.stats.length ? statList(md.stats.map(([k, v]) => [k, v])) : null,
    md.enh ? h("div", { class: "tcard-sec enh" }, h("div", { class: "tcard-enh" }, icon(md.enh.icon, "enh-ico"), md.enh.name),
      md.enh.desc ? h("div", { class: "mod-line" }, md.enh.desc) : null,
      md.enh.stats && md.enh.stats.length ? statList(md.enh.stats) : null) : null);
}
// A unique mod (enhancement) is what defines a build, so its own icon is the face of the tile; the slot glyph moves to
// the corner. A plain mod shows the slot glyph.
function modIcon(md) {
  if (md.empty) return h("div", { class: "modicon empty", title: `${md.slot}: empty` }, glyph(md.slot));
  const slot = () => icon(md.icon, "", glyph(md.slot));
  const el = h("div", { class: "modicon" + (md.enh ? " enh" : ""), style: `--qc:${QVAR[md.quality] || QVAR[0]}`,
    "aria-label": `${md.qname} ${md.slot_label || md.slot + " mod"}${md.enh ? ", " + md.enh.name : ""}` },
    md.enh && md.enh.icon ? [icon(md.enh.icon, "", glyph(md.slot)), h("span", { class: "modicon-slot" }, slot())] : slot());
  return hoverCard(el, () => modCard(md));
}
function modCell(md) {
  return h("div", { class: "modcell" }, modIcon(md), h("div", { class: "modcell-name" + (md.enh ? " enh" : "") }, md.empty ? "" : md.enh ? md.enh.name : md.slot));
}
function gearTile(g, opts) {
  const o = opts || {};
  if (g.empty) return h("div", { class: "gtile empty" + (o.wide ? " wide" : "") }, h("div", { class: "gtile-slot" }, g.slot), h("div", { class: "gtile-name" }, "Empty"));
  const el = h("div", { class: "gtile" + (o.wide ? " wide" : ""), style: `--qc:${QVAR[g.quality] || QVAR[0]}`, "aria-label": `${g.qname} ${g.name}` },
    h("div", { class: "gtile-slot" }, g.slot), h("i", { class: "gtile-corner" }),
    g.icon ? h("div", { class: "gtile-img" }, icon(g.icon)) : null,
    h("div", { class: "gtile-name" }, g.name), g.max_level ? pips(g.level || 0, g.max_level) : null);
  // an upgrade level only where the item can be upgraded (weapons and armour; not deployables), out of the steps
  // its crafting table has
  return hoverCard(el, () => h("div", { class: "tcard" },
    h("div", { class: "tcard-bar", style: `background:${QVAR[g.quality] || QVAR[0]}` }, g.qname),
    h("div", { class: "tcard-title" }, h("span", null, g.name), g.max_level ? h("b", null, `upgrade ${g.level || 0}/${g.max_level}`) : null),
    o.extra ? h("div", { class: "mod-line" }, o.extra) : null,
    g.stats && g.stats.length ? statList(g.stats) : null));
}
// a weapon from a supply crate: not part of anyone's loadout, so its card has the weapon's own base numbers
function powerCard(w) {
  return h("div", { class: "tcard" },
    h("div", { class: "tcard-bar", style: "background:var(--power)" }, w.qname || "Power weapon"),
    h("div", { class: "tcard-title" }, h("span", null, w.name), dtypeChip(w.dtype)),
    h("div", { class: "mod-line" }, [w.category, "from a supply crate", "base numbers"].filter(Boolean).join(" · ")),
    w.stats && w.stats.length ? statList(w.stats) : null);
}
function overviewRow(label, value) { return h("div", { class: "ov-row" }, h("span", null, label), h("b", null, value)); }
function buildSection(title, left, right) {
  return h("div", { class: "bsec" }, h("div", null, h("div", { class: "bsec-title" }, title), left),
    right ? h("div", null, h("div", { class: "bsec-title" }, title.replace(/ loadout$/i, "") + " overview"), right) : null);
}
function hexTile(url, text, cls) {
  return h("div", { class: "hex " + (cls || "") }, icon(url, "", text ? h("span", null, text) : null) || (text ? h("span", null, text) : null));
}
function initials(name) { return name.split(/\s+/).map(x => x[0]).join("").slice(0, 3).toUpperCase(); }
// a list where what you have and what you do not must be told apart at a glance: mark + weight + colour, not shade alone
function stateLine(text, on) {
  return h("div", { class: "state-line" + (on ? " on" : " off") }, h("i", { "aria-hidden": "true" }, on ? "✓" : "–"), h("span", null, text));
}

function buildTab(d, p) {
  const b = p.build;
  if (!b) return h("div", { class: "empty" }, "No build captured for this player.");
  if (b.version !== 8) return h("div", { class: "empty" }, b.note || "This log was recorded by an older version of the meter; its build cannot be shown in this layout.");
  const c = b.character, a = b.armour;
  const out = [h("div", { class: "bhead" }, h("span", { class: "name" }, c.name), h("span", { class: "sub" }, [c.cls, c.sub].filter(Boolean).join(" · ")),
    h("span", { class: "tag" }, `Level ${c.level}`))];

  // armour
  const t = a.totals;
  const resMax = t ? Math.max(50, ...t.resist.map(r => r[1])) : 50;
  out.push(buildSection("Armor loadout",
    h("div", { class: "tiles-row" }, a.pieces.map(g => gearTile(g))),
    h("div", { class: "ov" },
      t ? t.resist.map(([name, v]) => h("div", { class: "ov-meter" }, dtypeChip(name),
        h("div", { class: "meter" }, h("i", { style: `width:${Math.max(0, v / resMax * 100).toFixed(1)}%;background:${DTYPE_COLOR[name] || "var(--series-1)"}` })), h("b", null, `${Math.round(v)}%`))) : null,
      t ? overviewRow("Movement speed", `${t.move >= 0 ? "+" : ""}${t.move.toFixed(2)}%`) : null,
      h("div", { class: "ov-row" }, h("span", null, "Mod power"), h("b", null, String(a.mod_power))),
      h("div", { class: "mods-row" }, a.mods.map(modCell)),
      t ? h("div", { class: "mod-line" }, t.note) : null)));

  // weapons
  out.push(buildSection("Weapons",
    h("div", { class: "tiles-row" }, b.weapons.map(w => gearTile(w, { wide: true, extra: w.empty ? "" : [w.category, w.dtype].filter(Boolean).join(" · ") }))),
    h("div", { class: "ov" }, b.weapons.filter(w => !w.empty).map(w => h("div", { class: "ov-weapon" },
      h("div", { class: "ov-row" }, h("span", { class: "name" }, [w.category, w.name].filter(Boolean).join(" \\\\ ")), dtypeChip(w.dtype)),
      h("div", { class: "ov-row" }, h("span", null, "Mod power"), h("b", null, String(w.mod_power))),
      h("div", { class: "mods-row" }, w.mods.map(modCell)),
      w.stats.length ? h("div", { class: "ov-stats" }, w.stats.map(([k, v]) => h("div", null, h("span", null, k), h("b", null, v)))) : null)))));

  // deployables
  if (b.deployables.length) out.push(buildSection("Deployables", h("div", { class: "tiles-row" }, b.deployables.map(g => gearTile(g))), null));

  // ability loadout
  if (b.abilities.length) {
    out.push(h("div", { class: "bsec one" }, h("div", null, h("div", { class: "bsec-title" }, "Ability loadout"),
      h("div", { class: "abil-row" }, b.abilities.map((ab, i) => ab.empty ? h("div", { class: "abil" }, hexTile(null, "", "dim"), h("div", { class: "abil-name" }, "Empty")) :
        h("div", { class: "abil", style: `--ac:${ABILITY_HUES[i % ABILITY_HUES.length]}` },
          hoverCard(hexTile(ab.icon, initials(ab.name), "big ring"), () => h("div", { class: "tcard" },
            h("div", { class: "tcard-title" }, h("span", null, ab.name), ab.cooldown ? h("b", null, `${Number(ab.cooldown)}s cooldown`) : null),
            ab.desc ? h("div", { class: "mod-line" }, ab.desc) : null)),
          h("div", null, h("div", { class: "abil-name" }, ab.name),
            h("div", { class: "chips" }, ab.perks.map(pk => hoverCard(h("span", { class: "chip" + (pk.kind === "Enhancement" ? " enh" : "") }, icon(pk.icon, "chip-ico"), pk.name),
              () => h("div", { class: "tcard" }, h("div", { class: "tcard-title" }, h("span", null, pk.name), h("b", null, pk.kind)),
                pk.desc ? h("div", { class: "mod-line" }, pk.desc) : null)))))))))));
  }

  // skills
  for (const g of b.skills) {
    out.push(h("div", { class: "bsec one" }, h("div", null,
      h("div", { class: "bsec-title" }, g.title, h("span", { class: "sub" }, `  ${g.items.reduce((x, i) => x + i.rank, 0)} points`)),
      h("div", { class: "skill-row" }, g.items.map(sk => hoverCard(
        h("div", { class: "skill" + (sk.rank ? "" : " dim"), "aria-label": `${sk.name} rank ${sk.rank} of ${sk.max}` },
          h("div", { class: "skill-hex" }, hexTile(sk.icon, sk.rank ? String(sk.rank) : "", sk.rank ? "" : "dim"), sk.rank ? h("b", { class: "rank" }, String(sk.rank)) : null),
          h("div", { class: "skill-name" }, sk.name), pips(sk.rank, Math.max(sk.max, 1))),
        () => h("div", { class: "tcard" },
          h("div", { class: "tcard-title" }, h("span", null, sk.name), h("b", null, `rank ${sk.rank} of ${sk.max}`)),
          sk.desc ? h("div", { class: "mod-line" }, sk.desc) : null,
          sk.ranks.length ? h("div", { class: "tcard-sec" }, sk.ranks.map((r, i) => stateLine(r, i < sk.rank))) : null,
          sk.bonus ? stateLine(`Max rank: ${sk.bonus}`, sk.rank >= sk.max) : null)))))));
  }
  return h("div", { class: "buildv" }, out);
}

function captureNote(d) {
  const me = d.players.find(p => p.local);
  const bits = [`${fmtFull(d.stats.server_hits)} hits logged`];
  if (d.first_hit != null) bits.push(`first hit at ${timer(Math.max(0, d.first_hit))}, last at ${timer(d.last_hit)}`);
  if (me && me.native_total != null && me.native_total > 0) {
    // The game's own counter covers weapon damage and abilities that hit directly, structures included; it leaves out
    // abilities that work through something they place (Haven, Biocleanse Bomb, turrets) and blasts of barrels you
    // set off. So compare it with whichever of our two sums it is closer to.
    const direct = me.counter_direct != null ? me.counter_direct : me.direct_damage != null ? me.direct_damage : me.damage;
    const weapon = me.counter_weapon != null ? me.counter_weapon : me.weapon_damage != null ? me.weapon_damage : direct;
    const useWeapon = Math.abs(weapon - me.native_total) < Math.abs(direct - me.native_total);
    const mine = useWeapon ? weapon : direct;
    bits.push(`your ${useWeapon ? "weapon" : "weapon and ability"} damage with structures ${fmtFull(mine)} vs the game's own counter ${fmtFull(me.native_total)} (${pct(mine / me.native_total)})`);
  }
  if (d.stats.blast_hits) bits.push(`${fmtFull(d.stats.blast_hits)} hits of enemy explosions the game credits to players, counted only when switched on`);
  if (d.stats.enemy_hits) bits.push(d.blasts === "never"
    ? `${fmtFull(d.stats.enemy_hits)} hits by enemies the game credits to players, explosions included, were not recorded`
    : `${fmtFull(d.stats.enemy_hits)} other hits by enemies the game credits to players are not counted`);
  if (d.stats.late) bits.push(`${d.stats.late} hits read late`);
  return h("div", { class: "note" }, "Capture: " + bits.join(" · "));
}

function drawCharts(d) {
  const host = document.getElementById("chart-time");
  const sel = d.players.find(x => x.cid === ui.player);
  if (host) {
    const boss = (d.bosses || []).find(b => b.id === ui.target);
    const raw = pl => boss ? boss.series[String(pl.cid)] || [] : pl.series;
    const k = Math.ceil(Math.max(...d.players.map(pl => raw(pl).length), 1) / 600);
    const series = d.players.map(pl => {
      const base = ui.mode === "dps" ? rolling(raw(pl), Math.max(1, Math.round(ui.window / (d.bucket || 1)))) : cumulative(raw(pl));
      const [vals] = downsample(base, 600, ui.mode === "dps" ? "avg" : "last");
      return { name: pl.name, color: playerColor(d, pl.cid), values: vals };
    });
    // the selected player's abilities and effects, and with a boss chosen, the effects on it under them
    const own = playerRows(d, sel);
    const lanes = boss ? own.concat(bossRows(d, boss)) : own;
    // moments worth a line: every ability activation of the selected player, and bosses appearing / dying
    const markers = [];
    for (const ln of own) {
      if (ln.kind === "ability") for (const [a] of ln.intervals) markers.push({ t: a, label: ln.name, color: ln.color });
    }
    for (const ev of d.events || []) {
      if (ev.kind === "reset") {
        // a cooldown reset: only the selected player's (all of them in the party view)
        if (!sel || ev.cid === sel.cid) markers.push({ t: ev.t, label: ev.label, color: "var(--series-7)", full: true });
      } else markers.push({ t: ev.t, label: ev.label, color: "var(--series-8)", full: true });
    }
    let level = null;
    if (boss && boss.hp_series.some(v => v)) {
      const [vals] = downsample(boss.hp_series.map(v => (v ? v[0] + v[1] : null)), 600, "last");
      level = { name: "Health", values: vals, max: boss.hp_max || Math.max(...vals.filter(v => v != null), 1), color: "var(--series-8)" };
    }
    const from = boss ? boss.start : 0, to = boss ? Math.max(boss.end, boss.start + 2) : d.duration;
    timelineChart(host, { series, step: d.bucket * Math.max(1, k), anchor: ui.mode === "dps" ? 0.5 : 1, start: from, duration: to, lanes, markers, level,
      label: "Damage over time per player, with ability and effect timelines" });
  }
  const sh = document.getElementById("chart-src");
  const p = sel;
  if (sh && p && p.sources.length) {
    const n = Math.max(1, Math.ceil(d.duration / d.bucket));
    const step = [1, 2, 5, 10, 15, 30, 60, 120, 300].find(st => n / st <= 60) || 600;
    const cols = Math.ceil(n / step);
    const map = sourceSlots(d, p);
    const mk = (name, color) => ({ name, color, values: new Array(cols).fill(0) });
    const colored = p.sources.filter(x => x.key in map).sort((a, b) => map[a.key] - map[b.key]);
    const series = colored.map(x => mk(x.name, SERIES[map[x.key]]));
    const other = mk("Other", OTHER);
    p.sources.forEach(x => {
      const tgt = x.key in map ? series[colored.indexOf(x)] : other;
      for (const [sec, dmg] of x.series) { const c = Math.min(cols - 1, Math.floor(sec / step)); tgt.values[c] += dmg / step; }
    });
    if (other.values.some(v => v > 0)) series.push(other);
    const sub = document.getElementById("src-sub");
    if (sub) sub.textContent = `Damage per second by source, ${step}s columns`;
    stackedColumns(sh, { series, step: step * d.bucket, label: "Damage per second by source over time" });
  }
  const hh = document.getElementById("chart-hp");
  if (hh && p && p.hp_series) {
    let last = [0, 0];
    const filled = p.hp_series.map(v => (last = v || last));
    const [hp] = downsample(filled.map(v => v[0]), 600, "last");
    const [shd] = downsample(filled.map(v => v[1]), 600, "last");
    const k = Math.ceil(Math.max(filled.length, 1) / 600);
    lineChart(hh, { series: [{ name: "Health", color: "var(--series-3)", values: hp }, { name: "Shield", color: "var(--series-1)", values: shd }],
      step: d.bucket * Math.max(1, k), height: 180, label: "Health and shield over time" });
  }
}

async function removeEncounter(id) {
  if (!confirm("Delete this encounter log? This cannot be undone.")) return;
  await act(`/api/encounter/${encodeURIComponent(id)}`, "DELETE");
  ui.sel = null; ui.detail = null;
  await refreshList(); await refreshDetail(true);
}

// ---------- sharing logs ----------
// A mission's log is one file. To share it, the app shows that file in Explorer, selected: drag it from there. To
// add logs, the app asks which files with a Windows dialog of its own. The page never handles a path itself.
async function showInFolder(id) {
  try {
    if (!(await act(`/api/encounter/${encodeURIComponent(id)}/reveal`)).ok) notify("Could not show the file: it is no longer in the logs folder.");
  } catch (e) { notify("Could not show the file: " + e.message); }
}
async function importEncounters() {
  try {
    const r = await act("/api/import");
    if (r.cancelled) return;
    const n = (r.added || []).length, bits = [];
    bits.push(n ? `Imported ${n} log${n === 1 ? "" : "s"}.` : "Nothing was imported.");
    if (r.present) bits.push(`${r.present} ${r.present === 1 ? "was" : "were"} already here.`);
    for (const x of r.rejected || []) bits.push(`${x.file}: ${x.why}.`);
    await refreshList();
    if (n) { ui.sel = r.added[0]; ui.about = false; ui.records = false; ui.player = null; ui.target = null; renderList(); await refreshDetail(true); }
    notify(bits.join(" "));
  } catch (e) { notify("Could not import: " + e.message); }
}
async function setCounted(what, on) {
  try {
    await act(`/api/${what}/` + (on ? "on" : "off"));
    await refreshList(); await refreshDetail(true);
  } catch (e) { notify("Could not change the setting: " + e.message); }
}

// a refresh that arrived while something was being read is applied once the pointer has moved on
setInterval(() => { if (ui.stale && !window.chartHover && !window.cardHover) renderMain(); }, 500);

// ---------- boot ----------
let resizeTimer = 0;
addEventListener("resize", () => { clearTimeout(resizeTimer); resizeTimer = setTimeout(() => ui.detail && drawCharts(ui.detail), 120); });
(async function boot() {
  renderMain();
  try { await refreshList(); await refreshDetail(true); } catch (e) { /* server starting */ }
  const es = new EventSource("/api/stream");
  es.onmessage = ev => { try { onLive(JSON.parse(ev.data)); } catch (e) { console.error(e); } };
})();

document.getElementById("about-open").addEventListener("click", openAbout);
document.getElementById("records-open").addEventListener("click", openRecords);
document.getElementById("import-open").addEventListener("click", importEncounters);
