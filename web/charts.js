// Small dependency-free SVG charts. All text goes through textContent (player and item names are untrusted).
"use strict";

const SVGNS = "http://www.w3.org/2000/svg";

function h(tag, props, ...kids) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(props || {})) {
    if (v == null || v === false) continue;
    if (k === "class") e.className = v;
    else if (k === "style") e.style.cssText = v;
    else if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
    else e.setAttribute(k, v);
  }
  for (const c of kids.flat(3)) {
    if (c == null || c === false) continue;
    e.append(c.nodeType ? c : document.createTextNode(String(c)));
  }
  return e;
}

function s(tag, attrs, ...kids) {
  const e = document.createElementNS(SVGNS, tag);
  for (const [k, v] of Object.entries(attrs || {})) if (v != null) e.setAttribute(k, v);
  for (const c of kids.flat()) if (c != null) e.append(c.nodeType ? c : document.createTextNode(String(c)));
  return e;
}

function fmt(n, digits) {
  if (n == null || !isFinite(n)) return "–";
  const a = Math.abs(n);
  if (a >= 1e9) return (n / 1e9).toFixed(2) + "B";
  if (a >= 1e6) return (n / 1e6).toFixed(a >= 1e8 ? 0 : a >= 1e7 ? 1 : 2) + "M";
  if (a >= 1e4) return (n / 1e3).toFixed(a >= 1e5 ? 0 : 1) + "K";
  if (digits != null) return n.toFixed(digits);
  return Math.round(n).toLocaleString("en-US");
}
function fmtFull(n) { return n == null ? "–" : Math.round(n).toLocaleString("en-US"); }
function pct(x, d = 1) { return x == null || !isFinite(x) ? "–" : (x * 100).toFixed(d) + "%"; }
function clock(sec) {
  sec = Math.max(0, Math.round(sec));
  const m = Math.floor(sec / 60), ss = String(sec % 60).padStart(2, "0");
  return m >= 60 ? `${Math.floor(m / 60)}:${String(m % 60).padStart(2, "0")}:${ss}` : `${m}:${ss}`;
}

// mission timer, truncated like a stopwatch (never rounded up). The game shows milliseconds, so that is the default.
function timer(sec, digits = 3) {
  const k = Math.pow(10, digits);
  const t = Math.max(0, Math.floor(sec * k + 1e-6) / k), m = Math.floor(t / 60), r = t - m * 60;
  const ss = (r < 10 ? "0" : "") + r.toFixed(digits);
  return m >= 60 ? `${Math.floor(m / 60)}:${String(m % 60).padStart(2, "0")}:${ss}` : `${m}:${ss}`;
}

function niceTicks(max, count) {
  if (!(max > 0)) return [0, 1];
  const raw = max / count, mag = Math.pow(10, Math.floor(Math.log10(raw)));
  const step = [1, 2, 2.5, 5, 10].map(m => m * mag).find(st => st >= raw);
  const out = [];
  for (let v = 0; v <= max + step * 0.999; v += step) out.push(v);
  return out;
}
function timeTicks(total, width) {
  const steps = [5, 10, 15, 30, 60, 120, 300, 600, 900, 1800, 3600];
  const want = Math.max(2, Math.floor(width / 90));
  const step = steps.find(st => total / st <= want) || 7200;
  const out = [];
  for (let v = 0; v <= total + 1e-6; v += step) out.push(v);
  return out;
}

// ---- tooltip (one shared element) ----
const Tip = {
  el: null,
  show(evt, head, rows) {
    const el = this.el || (this.el = document.getElementById("tooltip"));
    el.replaceChildren();
    if (head) el.append(h("div", { class: "tt-head" }, head));
    for (const r of rows) {
      if (r.rule) { el.append(h("div", { class: "tt-rule" })); continue; }
      // a state row (r.dot): filled dot + full ink when on, hollow dot + dimmed when off
      el.append(h("div", { class: "tt-row" + (r.off ? " off" : "") },
        h("span", { class: "tt-key" },
          r.color ? h("i", { style: `background:${r.color}` }) : null,
          r.dot ? h("i", { class: "dot", style: r.off ? `box-shadow:inset 0 0 0 1.5px ${r.dot}` : `background:${r.dot}` }) : null, r.name),
        h("b", null, r.value)));
    }
    el.hidden = false;
    const pad = 14, w = el.offsetWidth, hh = el.offsetHeight;
    let x = evt.clientX + pad, y = evt.clientY + pad;
    if (x + w > innerWidth - 8) x = evt.clientX - w - pad;
    if (y + hh > innerHeight - 8) y = evt.clientY - hh - pad;
    el.style.left = Math.max(8, x) + "px";
    el.style.top = Math.max(8, y) + "px";
  },
  // a ready-made card (mod, gear, skill) instead of rows
  card(evt, node) {
    const el = this.el || (this.el = document.getElementById("tooltip"));
    el.replaceChildren(node);
    el.hidden = false;
    const pad = 14, w = el.offsetWidth, hh = el.offsetHeight;
    let x = evt.clientX + pad, y = evt.clientY + pad;
    if (x + w > innerWidth - 8) x = evt.clientX - w - pad;
    if (y + hh > innerHeight - 8) y = Math.max(8, innerHeight - hh - 8);
    el.style.left = Math.max(8, x) + "px";
    el.style.top = Math.max(8, y) + "px";
  },
  hide() { if (this.el) this.el.hidden = true; },
};

window.chartHover = false;   // the page skips live re-renders while a chart is being read
window.cardHover = false;    // ... or while a hover card is open

// ---- multi-series line chart over time ----
// opts: { series: [{name, color, values: number[]}], step: seconds per point, yFormat, height }
function lineChart(host, opts) {
  const W = Math.max(320, host.clientWidth), H = opts.height || 260;
  const m = { l: 48, r: 16, t: 8, b: 24 };
  const n = Math.max(...opts.series.map(x => x.values.length), 1);
  const total = n * opts.step;
  const ymax = Math.max(...opts.series.map(x => Math.max(0, ...x.values)), 1);
  const yt = niceTicks(ymax, 4), ytop = yt[yt.length - 1];
  const X = i => m.l + (W - m.l - m.r) * (n > 1 ? i / (n - 1) : 0);
  const Y = v => m.t + (H - m.t - m.b) * (1 - v / ytop);
  const svg = s("svg", { viewBox: `0 0 ${W} ${H}`, height: H, role: "img", "aria-label": opts.label || "Line chart" });
  for (const v of yt) {
    svg.append(s("line", { class: v === 0 ? "axis" : "grid", x1: m.l, x2: W - m.r, y1: Y(v), y2: Y(v) }));
    svg.append(s("text", { x: m.l - 8, y: Y(v) + 4, "text-anchor": "end" }, fmt(v)));
  }
  for (const t of timeTicks(total, W - m.l - m.r)) {
    const x = m.l + (W - m.l - m.r) * (t / total);
    if (x > W - m.r + 1) continue;
    svg.append(s("text", { x, y: H - 6, "text-anchor": "middle" }, clock(t)));
  }
  for (const ser of opts.series) {
    if (!ser.values.length) continue;
    let d = "";
    ser.values.forEach((v, i) => { d += (i ? "L" : "M") + X(i).toFixed(1) + " " + Y(v).toFixed(1); });
    svg.append(s("path", { d, fill: "none", stroke: ser.color, "stroke-width": 2, "stroke-linejoin": "round", "stroke-linecap": "round" }));
  }
  const cross = s("line", { class: "cross", y1: m.t, y2: H - m.b, visibility: "hidden" });
  const dots = opts.series.map(ser => s("circle", { r: 4, fill: ser.color, stroke: "var(--surface-1)", "stroke-width": 2, visibility: "hidden" }));
  svg.append(cross, ...dots);
  const hit = s("rect", { x: m.l, y: m.t, width: W - m.l - m.r, height: H - m.t - m.b, fill: "transparent" });
  svg.append(hit);
  hit.addEventListener("pointermove", e => {
    const r = hit.getBoundingClientRect();
    const i = Math.max(0, Math.min(n - 1, Math.round((e.clientX - r.left) / r.width * (n - 1))));
    cross.setAttribute("x1", X(i)); cross.setAttribute("x2", X(i)); cross.setAttribute("visibility", "visible");
    opts.series.forEach((ser, k) => {
      const v = ser.values[i];
      dots[k].setAttribute("visibility", v == null ? "hidden" : "visible");
      if (v != null) { dots[k].setAttribute("cx", X(i)); dots[k].setAttribute("cy", Y(v)); }
    });
    window.chartHover = true;
    Tip.show(e, clock(i * opts.step), opts.series.map(ser => ({ name: ser.name, color: ser.color, value: (opts.yFormat || fmt)(ser.values[i] ?? 0) })));
  });
  hit.addEventListener("pointerleave", () => {
    cross.setAttribute("visibility", "hidden"); dots.forEach(d => d.setAttribute("visibility", "hidden"));
    window.chartHover = false; Tip.hide();
  });
  host.replaceChildren(svg);
}

// ---- line chart with timeline lanes on one shared time axis ----
// The lanes (ability uses, status effects) sit directly under the plot and use the same x scale, so what was active
// when the damage moved can be read straight down a vertical line.
// opts: { series: [{name, color, values}], step, anchor (0.5 = bucket centre, 1 = bucket end), duration,
//         lanes: [{group, name, indent, kind, color, intervals: [[a, b]], cooldowns: [[a, b]], uptime, applications, timed}],
//         markers: [{t, label, color, full}]   event lines (dashed: they mark moments, they are not gridlines),
//         level: {name, values, max, color}    one quantity drawn as an area lane (a boss's health),
//         start                                first second shown (default 0); `duration` is the last. Everything is
//                                              clipped to that window, so a boss's chart runs from spawn to death,
//         yFormat, label }
function timelineChart(host, opts) {
  const W = Math.max(360, host.clientWidth);
  const G = Math.round(Math.max(128, Math.min(220, W * 0.22)));   // label gutter shared by the y axis and the lane names
  const w0 = Math.max(0, opts.start || 0), w1 = Math.max(opts.duration || 0, w0);
  const markers = (opts.markers || []).filter(mk => mk.t >= w0 - 1e-6 && mk.t <= w1 + 1e-6).sort((a, b) => a.t - b.t);
  const m = { l: G, r: 16, t: markers.length ? 34 : 8 };
  const plotH = 220, axisH = 24, rowH = 18, headH = 22, levelH = opts.level ? 46 : 0;
  const T0 = Math.max(0, opts.start || 0);
  const T = Math.max(opts.duration || 0, T0 + opts.step, 1e-6);
  const pw = W - m.l - m.r;
  const lanes = opts.lanes || [];
  let lanesH = 0, prevGroup = null;
  const rows = [];
  for (const ln of lanes) {
    if (ln.group !== prevGroup) { rows.push({ y: lanesH, head: ln.groupLabel || ln.group }); lanesH += headH; prevGroup = ln.group; }
    rows.push({ y: lanesH, lane: ln });
    lanesH += rowH;
  }
  const below = levelH + lanesH;
  const H = m.t + plotH + axisH + below + (below ? 6 : 0);
  const n = Math.max(...opts.series.map(x => x.values.length), 1);
  const anchor = opts.anchor == null ? 0.5 : opts.anchor;
  const inWin = i => { const t = (i + anchor) * opts.step; return t > T0 - opts.step && t < T + opts.step; };
  const ymax = Math.max(...opts.series.map(x => Math.max(0, ...x.values.filter((_v, i) => inWin(i)))), 1);
  const yt = niceTicks(ymax, 4), ytop = yt[yt.length - 1];
  const Xr = t => m.l + pw * (t - T0) / (T - T0);                 // unclamped: lines run to the edge and are clipped
  const X = t => m.l + pw * Math.max(0, Math.min(1, (t - T0) / (T - T0)));
  const Xi = i => Xr((i + anchor) * opts.step);
  const Y = v => m.t + plotH * (1 - v / ytop);
  const levelTop = m.t + plotH + axisH;
  const lanesTop = levelTop + levelH;
  const bottom = below ? lanesTop + lanesH : m.t + plotH;
  const svg = s("svg", { viewBox: `0 0 ${W} ${H}`, height: H, role: "img", "aria-label": opts.label || "Timeline" });
  const clipId = "clip" + Math.random().toString(36).slice(2, 9);
  svg.append(s("defs", null, s("clipPath", { id: clipId }, s("rect", { x: m.l, y: 0, width: pw, height: H }))));
  const clipped = s("g", { "clip-path": `url(#${clipId})` });

  for (const v of yt) {
    svg.append(s("line", { class: v === 0 ? "axis" : "grid", x1: m.l, x2: W - m.r, y1: Y(v), y2: Y(v) }));
    svg.append(s("text", { x: m.l - 8, y: Y(v) + 4, "text-anchor": "end" }, fmt(v)));
  }
  const rel = timeTicks(T - T0, pw), tstep = rel.length > 1 ? rel[1] - rel[0] : T - T0;
  for (let t = Math.ceil(T0 / tstep - 1e-9) * tstep; t <= T + 1e-6; t += tstep) {
    const x = X(t), first = x <= m.l + 12;
    if (below && !first) svg.append(s("line", { class: "grid", x1: x, x2: x, y1: levelTop, y2: bottom }));
    svg.append(s("text", { x, y: m.t + plotH + 16, "text-anchor": first ? "start" : "middle" }, clock(t)));
  }

  // event markers: a dashed line per moment, its label in one of two rows above the plot. Labels that would run
  // into one another are left out (the line stays); boss events get their label first.
  const taken = [[], []];
  const free = (row, x0, x1) => taken[row].every(([a, z]) => x1 <= a || x0 >= z);
  for (const mk of markers) {
    const x = X(mk.t);
    svg.append(s("line", { x1: x, x2: x, y1: m.t - 4, y2: mk.full ? bottom : m.t + plotH, stroke: mk.color || "var(--muted)",
      "stroke-width": mk.full ? 1.5 : 1, "stroke-dasharray": "4 3", opacity: mk.full ? 0.95 : 0.8 }));
  }
  for (const mk of markers.slice().sort((a, b) => (b.full ? 1 : 0) - (a.full ? 1 : 0) || a.t - b.t)) {
    const wLabel = mk.label.length * 6.2 + 6;
    const tx = Math.max(m.l, Math.min(X(mk.t) + 3, W - m.r - wLabel));
    const row = free(0, tx, tx + wLabel) ? 0 : free(1, tx, tx + wLabel) ? 1 : -1;
    if (row < 0) continue;
    taken[row].push([tx - 4, tx + wLabel + 4]);
    svg.append(s("text", { x: tx, y: 11 + row * 12, class: "mark-label" }, mk.label));
  }

  svg.append(clipped);
  for (const ser of opts.series) {
    if (!ser.values.length) continue;
    let d = "";
    ser.values.forEach((v, i) => { if (inWin(i)) d += (d ? "L" : "M") + Xi(i).toFixed(1) + " " + Y(v).toFixed(1); });
    if (d) clipped.append(s("path", { d, fill: "none", stroke: ser.color, "stroke-width": 2, "stroke-linejoin": "round", "stroke-linecap": "round" }));
  }

  const lv = opts.level;
  if (lv) {
    const top = levelTop + 6, hgt = levelH - 14, base = top + hgt;
    const LY = v => base - hgt * Math.max(0, Math.min(1, v / (lv.max || 1)));
    svg.append(s("text", { x: 4, y: top + hgt / 2 + 4, class: "lane-name" }, lv.name));
    svg.append(s("text", { x: m.l - 8, y: top + hgt / 2 + 4, "text-anchor": "end" }, fmt(lv.max)));
    svg.append(s("line", { class: "axis", x1: m.l, x2: W - m.r, y1: base, y2: base }));
    let d = "", started = false, lastX = 0;
    lv.values.forEach((v, i) => {
      if (v == null || !inWin(i)) return;
      const x = Xi(i);
      d += (started ? "L" : `M${x.toFixed(1)} ${base}L`) + x.toFixed(1) + " " + LY(v).toFixed(1);
      started = true; lastX = x;
    });
    if (started) {
      clipped.append(s("path", { d: d + `L${lastX.toFixed(1)} ${base}Z`, fill: lv.color, opacity: 0.18 }));
      clipped.append(s("path", { d: d.replace(/^M-?[\d.]+ [\d.]+L/, "M"), fill: "none", stroke: lv.color, "stroke-width": 2, "stroke-linejoin": "round" }));
    }
  }

  const maxChars = ind => Math.max(6, Math.floor((G - 54 - ind * 12) / 6.4));
  for (const r of rows) {
    const y = lanesTop + r.y;
    if (r.head) { svg.append(s("text", { x: 0, y: y + 15, class: "lane-head" }, r.head.toUpperCase())); continue; }
    const ln = r.lane, mid = y + rowH / 2, ind = ln.indent || 0, color = ln.color || "var(--text-secondary)";
    const cut = maxChars(ind), name = ln.name.length > cut ? ln.name.slice(0, cut - 1) + "…" : ln.name;
    if (!ind) svg.append(s("rect", { x: 0, y: mid - 4, width: 3, height: 8, rx: 1.5, fill: color }));
    const label = s("text", { x: 8 + ind * 12, y: mid + 4, class: ind ? "lane-sub" : "lane-name" }, name);
    label.append(s("title", null, ln.name + (ln.source ? ` (${ln.source})` : "")));
    svg.append(label);
    svg.append(s("text", { x: m.l - 8, y: mid + 4, "text-anchor": "end" },
      ln.kind === "ability" ? `${ln.applications}×` : pct(ln.uptime, 0)));
    svg.append(s("rect", { x: m.l, y: mid - 4, width: pw, height: 8, rx: 2, fill: "var(--grid)", opacity: 0.55 }));
    for (const [a, z] of ln.cooldowns || []) {
      if (z > a && z >= T0 && a <= T) svg.append(s("rect", { x: X(a), y: mid - 1, width: Math.max(0, X(z) - X(a)), height: 2, fill: color, opacity: 0.4 }));
    }
    for (const [a, z] of ln.intervals) {
      if (z < T0 || a > T) continue;
      const x0 = X(a), w = Math.max(2, X(z) - x0);
      svg.append(s("rect", { x: x0, y: mid - 4, width: w, height: 8, rx: 2, fill: color, opacity: ind ? 0.6 : 1 }));
    }
  }

  const cross = s("line", { class: "cross", y1: m.t, y2: bottom, visibility: "hidden" });
  const dots = opts.series.map(ser => s("circle", { r: 4, fill: ser.color, stroke: "var(--surface-1)", "stroke-width": 2, visibility: "hidden" }));
  svg.append(cross, ...dots);
  const hit = s("rect", { x: m.l, y: m.t, width: pw, height: H - m.t, fill: "transparent" });
  svg.append(hit);
  hit.addEventListener("pointermove", e => {
    const r = hit.getBoundingClientRect();
    const t = Math.max(T0, Math.min(T, T0 + (e.clientX - r.left) / r.width * (T - T0)));
    const i = Math.max(0, Math.min(n - 1, Math.round(t / opts.step - anchor)));
    const x = X(t);
    cross.setAttribute("x1", x); cross.setAttribute("x2", x); cross.setAttribute("visibility", "visible");
    opts.series.forEach((ser, k) => {
      const v = ser.values[i];
      dots[k].setAttribute("visibility", v == null ? "hidden" : "visible");
      if (v != null) { dots[k].setAttribute("cx", X((i + anchor) * opts.step)); dots[k].setAttribute("cy", Y(v)); }
    });
    const out = opts.series.map(ser => ({ name: ser.name, color: ser.color, value: (opts.yFormat || fmt)(ser.values[i] ?? 0) }));
    if (lv && lv.values[i] != null) out.push({ name: lv.name, color: lv.color, value: `${fmt(lv.values[i])} · ${pct(lv.values[i] / (lv.max || 1), 0)}` });
    // every named lane with its state at this moment: active ones first and in colour, the rest dimmed
    const on = ln => ln.intervals.some(([a, z]) => t >= a && t <= Math.max(z, a + 0.25));
    const shown = lanes.filter(ln => !ln.quiet || on(ln));
    const act = shown.filter(on), idle = shown.filter(ln => !on(ln));
    if (shown.length) out.push({ rule: true });
    for (const ln of act.slice(0, 12)) out.push({ name: (ln.indent ? "· " : "") + ln.name, dot: ln.color, value: "active" });
    for (const ln of idle.slice(0, Math.max(0, 12 - act.length))) out.push({ name: (ln.indent ? "· " : "") + ln.name, dot: ln.color, off: true, value: "off" });
    window.chartHover = true;
    Tip.show(e, timer(t, 1) + (T0 > 0 ? ` · ${timer(t - T0, 1)} in` : ""), out);
  });
  hit.addEventListener("pointerleave", () => {
    cross.setAttribute("visibility", "hidden"); dots.forEach(d => d.setAttribute("visibility", "hidden"));
    window.chartHover = false; Tip.hide();
  });
  host.replaceChildren(svg);
}

// ---- stacked columns over time ----
// opts: { series: [{name, color, values: number[]}], step: seconds per column, yFormat, height }
function stackedColumns(host, opts) {
  const W = Math.max(320, host.clientWidth), H = opts.height || 220;
  const m = { l: 48, r: 16, t: 8, b: 24 };
  const n = Math.max(...opts.series.map(x => x.values.length), 1);
  const totals = Array.from({ length: n }, (_, i) => opts.series.reduce((a, x) => a + (x.values[i] || 0), 0));
  const yt = niceTicks(Math.max(...totals, 1), 4), ytop = yt[yt.length - 1];
  const band = (W - m.l - m.r) / n, bw = Math.max(1, Math.min(24, band - 2));
  const Y = v => m.t + (H - m.t - m.b) * (1 - v / ytop);
  const svg = s("svg", { viewBox: `0 0 ${W} ${H}`, height: H, role: "img", "aria-label": opts.label || "Stacked column chart" });
  for (const v of yt) {
    svg.append(s("line", { class: v === 0 ? "axis" : "grid", x1: m.l, x2: W - m.r, y1: Y(v), y2: Y(v) }));
    svg.append(s("text", { x: m.l - 8, y: Y(v) + 4, "text-anchor": "end" }, fmt(v)));
  }
  for (const t of timeTicks(n * opts.step, W - m.l - m.r)) {
    const x = m.l + (W - m.l - m.r) * (t / (n * opts.step));
    if (x > W - m.r + 1) continue;
    svg.append(s("text", { x, y: H - 6, "text-anchor": "middle" }, clock(t)));
  }
  const gap = bw >= 6 ? 2 : 0;
  for (let i = 0; i < n; i++) {
    let acc = 0;
    const x = m.l + band * i + (band - bw) / 2;
    const live = opts.series.filter(ser => (ser.values[i] || 0) > 0);
    live.forEach((ser, k) => {
      const v = ser.values[i], y1 = Y(acc + v), y0 = Y(acc);
      acc += v;
      const hgt = y0 - y1 - (k < live.length - 1 ? gap : 0);
      if (hgt < 0.5) return;
      const top = k === live.length - 1;
      const r = top ? Math.min(4, bw / 2, hgt) : 0;
      const yy = y0 - (y0 - y1);
      // rounded data-end on the top segment only, square at the baseline
      const d = `M${x} ${yy + hgt}V${yy + r}Q${x} ${yy} ${x + r} ${yy}H${x + bw - r}Q${x + bw} ${yy} ${x + bw} ${yy + r}V${yy + hgt}Z`;
      svg.append(s("path", { d, fill: ser.color }));
    });
    const hit = s("rect", { x: m.l + band * i, y: m.t, width: band, height: H - m.t - m.b, fill: "transparent" });
    hit.addEventListener("pointermove", e => {
      window.chartHover = true;
      Tip.show(e, `${clock(i * opts.step)} – ${clock((i + 1) * opts.step)}`,
        opts.series.filter(ser => (ser.values[i] || 0) > 0).map(ser => ({ name: ser.name, color: ser.color, value: (opts.yFormat || fmt)(ser.values[i]) })).reverse());
    });
    hit.addEventListener("pointerleave", () => { window.chartHover = false; Tip.hide(); });
    svg.append(hit);
  }
  host.replaceChildren(svg);
}

// ---- tall segmented bar: one whole split into <= 6 parts ----
// parts: [{name, value, color, darkInk}]
function segmentedBar(parts) {
  const total = parts.reduce((a, p) => a + p.value, 0) || 1;
  const bar = h("div", { class: "segbar", role: "img", "aria-label": parts.map(p => `${p.name} ${pct(p.value / total)}`).join(", ") });
  for (const p of parts) {
    if (p.value <= 0) continue;
    const share = p.value / total;
    const seg = h("div", { class: p.darkInk ? "dark-ink" : "", style: `flex:${p.value} 1 0; background:${p.color}` },
      share >= 0.07 ? pct(share, 0) : "");
    seg.addEventListener("pointermove", e => Tip.show(e, null, [{ name: p.name, color: p.color, value: `${fmt(p.value)} · ${pct(share)}` }]));
    seg.addEventListener("pointerleave", () => Tip.hide());
    bar.append(seg);
  }
  return bar;
}

function legend(items, line) {
  return h("div", { class: "legend" }, items.map(it => h("span", null, h("i", { class: "swatch" + (line ? " line" : ""), style: `background:${it.color}` }), it.name)));
}
