/* Charts for "Two weeks of the factory". Each chart is a custom element placed in the post's
   Markdown; they share one data file (data.json next to this script, made by the factory's
   extraction script). Plain DOM and SVG, no libraries. Labels go in with textContent. */
(() => {
  const BASE = "/blog-media/two-weeks-of-the-factory/";
  const NS = "http://www.w3.org/2000/svg";
  let dataPromise;
  const data = () => (dataPromise ??= fetch(BASE + "data.json?v=2").then((r) => r.json()));

  // Seven role groups, in the validated categorical order (stacks follow it).
  const GROUPS = [
    { id: "reviewer", label: "Reviewer", color: "#2a78d6", roles: ["reviewer"] },
    { id: "implementer", label: "Implementer", color: "#eb6834", roles: ["implementer"] },
    { id: "integrator", label: "Integrator", color: "#1baf7a", roles: ["integrator"] },
    { id: "triager", label: "Triager", color: "#eda100", roles: ["triager"] },
    { id: "dogfooder", label: "Dogfooder", color: "#e87ba4", roles: ["dogfooder"] },
    { id: "loops", label: "Quality, speed, scout", color: "#008300", roles: ["quality", "speed", "scout", "benchmark"] },
    { id: "planner", label: "Planner", color: "#6250d6", roles: ["planner"] },
  ];
  const groupOfRole = Object.fromEntries(GROUPS.flatMap((g, i) => g.roles.map((r) => [r, i])));
  const WHY = ["first build", "fixing review feedback", "landing on main", "pushing for Bugbot", "steered", "retry"];
  const MODEL_LABEL = { "composer-2.5": "Composer 2.5", "gpt-5.6-sol": "GPT-5.6 Sol", "claude-opus-5-5": "Claude Opus 5.5", "claude-sonnet-5-5": "Claude Sonnet 5.5" };

  // ---------------------------------------------------------------------------
  // Helpers

  function svg(tag, attrs = {}, parent) {
    const e = document.createElementNS(NS, tag);
    for (const [k, v] of Object.entries(attrs)) if (v != null) e.setAttribute(k, String(v));
    if (parent) parent.appendChild(e);
    return e;
  }
  function html(tag, attrs = {}, parent, text) {
    const e = document.createElement(tag);
    for (const [k, v] of Object.entries(attrs)) if (v != null) e.setAttribute(k, String(v));
    if (text != null) e.textContent = text;
    if (parent) parent.appendChild(e);
    return e;
  }
  const nf = new Intl.NumberFormat("en-US");
  const num = (n) => nf.format(Math.round(n));
  const big = (n) => (n >= 1e9 ? `${(n / 1e9).toFixed(1)}B` : n >= 1e6 ? `${(n / 1e6).toFixed(n >= 1e8 ? 0 : 1)}M` : n >= 1e3 ? `${(n / 1e3).toFixed(0)}k` : String(Math.round(n)));
  const dur = (min) => (min >= 120 ? `${(min / 60).toFixed(1)} h` : `${Math.round(min)} min`);
  const at = (d, min) => new Date(Date.parse(d.start) + min * 60000);
  const dayLabel = (date) => date.toLocaleDateString("en-US", { month: "short", day: "numeric", timeZone: "UTC" });
  const stamp = (date) =>
    `${dayLabel(date)}, ${date.toLocaleTimeString("en-US", { hour: "numeric", minute: "2-digit", timeZone: "UTC" })} UTC`;
  const hash = (n) => {
    let x = (n * 2654435761) >>> 0;
    x ^= x >>> 15;
    return (x >>> 0) / 4294967296;
  };

  // One tooltip for every chart. Values lead, labels follow.
  let tip;
  function showTip(evt, rows) {
    tip ??= html("div", { class: "fx-tip", role: "status" }, document.body);
    tip.replaceChildren();
    for (const r of rows) {
      const line = html("div", { class: r.head ? "fx-tip-head" : "fx-tip-row" }, tip);
      if (r.color) html("span", { class: "fx-key", style: `background:${r.color}` }, line);
      if (r.value != null) html("strong", {}, line, r.value);
      if (r.label != null) html("span", {}, line, r.label);
    }
    tip.hidden = false;
    const pad = 14;
    const w = tip.offsetWidth, h = tip.offsetHeight;
    let x = evt.clientX + pad, y = evt.clientY + pad;
    if (x + w > window.innerWidth - 8) x = evt.clientX - w - pad;
    if (y + h > window.innerHeight - 8) y = evt.clientY - h - pad;
    tip.style.left = `${Math.max(8, x)}px`;
    tip.style.top = `${Math.max(8, y)}px`;
  }
  const hideTip = () => tip && (tip.hidden = true);

  function legend(parent, items) {
    const l = html("div", { class: "fx-legend" }, parent);
    for (const it of items) {
      const s = html("span", {}, l);
      html("span", { class: `fx-swatch${it.line ? " line" : ""}`, style: `background:${it.color}` }, s);
      s.appendChild(document.createTextNode(it.label));
    }
    return l;
  }

  function segmented(parent, options, value, onChange, label) {
    const g = html("div", { class: "fx-seg", role: "group", "aria-label": label }, parent);
    const buttons = options.map(([v, text]) => {
      const b = html("button", { type: "button", "aria-pressed": String(v === value) }, g, text);
      b.addEventListener("click", () => {
        for (const o of buttons) o.setAttribute("aria-pressed", String(o === b));
        onChange(v);
      });
      return b;
    });
    return g;
  }

  function table(parent, caption, head, rows) {
    const det = html("details", { class: "fx-data" }, parent);
    html("summary", {}, det, caption);
    const wrap = html("div", { class: "fx-table" }, det);
    const t = html("table", {}, wrap);
    const tr = html("tr", {}, html("thead", {}, t));
    for (const h of head) html("th", {}, tr, h);
    const tb = html("tbody", {}, t);
    for (const r of rows) {
      const row = html("tr", {}, tb);
      for (const c of r) html("td", {}, row, c);
    }
  }

  /** A chart element: loads the data, renders at its width, re-renders on resize. */
  class Chart extends HTMLElement {
    connectedCallback() {
      if (this.started) return;
      this.started = true;
      this.classList.add("fx");
      data()
        .then((d) => {
          this.d = d;
          this.replaceChildren();
          this.setup(d);
          this.draw();
          let last = this.clientWidth;
          new ResizeObserver(() => {
            if (Math.abs(this.clientWidth - last) < 8) return;
            last = this.clientWidth;
            this.draw();
          }).observe(this);
        })
        .catch(() => {
          this.classList.add("fx-failed");
        });
    }
    setup() {}
    draw() {}
  }

  // ---------------------------------------------------------------------------
  // Headline numbers

  class Stats extends Chart {
    setup(d) {
      const t = d.totals;
      const days = Math.round((Date.parse(d.end) - Date.parse(d.start)) / 86400000);
      const tiles = [
        [num(t.tasks), "agents launched", `${num(t.runs)} runs over ${days} days`],
        [num(t.agentHours), "agent-hours", `about ${Math.round(t.agentHours / days / 24)} agents working at any moment`],
        [big(t.tokens), "tokens", `${Math.round((t.cacheTokens / t.tokens) * 100)}% of them cached reads`],
        [num(t.reviews), "code reviews", "each by a different model than the author"],
        [num(t.integrated), "changes landed", `${num(t.commits)} commits on main`],
        [`+${big(t.linesAdded)}`, "lines added", `−${big(t.linesRemoved)} removed`],
        [num(t.releases), "releases", "one every day, checked on 3 platforms"],
        [num(t.issuesClosed), "issues closed", `${num(t.triaged)} triaged, ${num(t.issuesFiledByAgents)} filed by agents`],
      ];
      const grid = html("div", { class: "fx-stats" }, this);
      for (const [value, label, sub] of tiles) {
        const tile = html("div", { class: "fx-stat" }, grid);
        html("strong", {}, tile, value);
        html("span", {}, tile, label);
        html("small", {}, tile, sub);
      }
    }
  }

  // ---------------------------------------------------------------------------
  // The heartbeat: every agent run, two weeks, one line each

  class Heartbeat extends Chart {
    setup(d) {
      this.canvas = html("canvas", { class: "fx-canvas", role: "img", "aria-label": `Timeline of all ${d.totals.runs} agent runs by role` }, this);
      legend(this, [
        ...GROUPS.map((g) => ({ label: g.label, color: g.color })),
        { label: "landed on main", color: "var(--ink)" },
      ]);
      this.lanes = [6, 1, 0, 2, 3, 4, 5]; // planner, implementer, reviewer, integrator, triager, dogfooder, loops
      const per = this.lanes.map(() => []);
      for (const r of d.runs) per[this.lanes.indexOf(groupOfRole[d.roles[r[2]]])].push(r);
      this.byLane = per;
      const days = {};
      for (const r of d.runs) {
        const key = at(d, r[0]).toISOString().slice(0, 10);
        days[key] ??= GROUPS.map(() => 0);
        days[key][groupOfRole[d.roles[r[2]]]]++;
      }
      table(this, "Runs per day by role", ["Day", ...GROUPS.map((g) => g.label)], Object.entries(days).map(([k, v]) => [k, ...v.map(num)]));
      this.canvas.addEventListener("pointermove", (e) => this.hover(e));
      this.canvas.addEventListener("pointerleave", hideTip);
    }
    layout() {
      const W = this.clientWidth;
      const left = W < 520 ? 74 : 104;
      const heights = this.lanes.map((g) => (g === 0 || g === 1 ? (W < 520 ? 92 : 120) : 26));
      const top = 30, landed = 36;
      const ys = [];
      let y = top;
      for (const h of heights) {
        ys.push(y);
        y += h + 6;
      }
      return { W, left, right: 8, top, heights, ys, landedY: y + 4, H: y + 4 + landed + 18 };
    }
    draw() {
      const d = this.d;
      const L = (this.L = this.layout());
      const dpr = window.devicePixelRatio || 1;
      const c = this.canvas;
      c.width = L.W * dpr;
      c.height = L.H * dpr;
      c.style.height = `${L.H}px`;
      const g = c.getContext("2d");
      g.scale(dpr, dpr);
      const css = getComputedStyle(this);
      const ink = css.getPropertyValue("--ink").trim() || "#1b1915";
      const muted = css.getPropertyValue("--muted").trim() || "#7b7466";
      const line = css.getPropertyValue("--line").trim() || "#d9d3c5";
      const span = (Date.parse(d.end) - Date.parse(d.start)) / 60000;
      this.span = span;
      const x = (m) => L.left + (m / span) * (L.W - L.left - L.right);
      this.x = x;
      g.font = `11px ${css.getPropertyValue("--mono") || "monospace"}`;
      // Days
      g.fillStyle = muted;
      g.strokeStyle = line;
      g.lineWidth = 1;
      const start = new Date(d.start);
      const firstDay = Date.UTC(start.getUTCFullYear(), start.getUTCMonth(), start.getUTCDate()) + 86400000;
      const dayEvery = L.W < 520 ? 3 : 2;
      let i = 0;
      for (let t = firstDay; t < Date.parse(d.end); t += 86400000, i++) {
        const xx = Math.round(x((t - Date.parse(d.start)) / 60000)) + 0.5;
        g.beginPath();
        g.moveTo(xx, L.top - 4);
        g.lineTo(xx, L.landedY + 36);
        g.stroke();
        if (i % dayEvery === 0 && xx + 48 < L.W) g.fillText(dayLabel(new Date(t)), xx + 3, 12);
      }
      // Releases
      g.fillStyle = ink;
      for (const r of d.releases) {
        const xx = x(r.at);
        g.beginPath();
        g.moveTo(xx - 3, 18);
        g.lineTo(xx + 3, 18);
        g.lineTo(xx, 24);
        g.fill();
      }
      g.fillStyle = muted;
      g.fillText("releases ▾", 4, 24);
      // Lanes
      this.lanes.forEach((gi, li) => {
        const y0 = L.ys[li], h = L.heights[li];
        g.fillStyle = ink;
        g.fillText(L.W < 520 ? GROUPS[gi].label.split(",")[0].slice(0, 10) : GROUPS[gi].label.split(",")[0], 4, y0 + Math.min(h, 26) / 2 + 4);
        g.strokeStyle = GROUPS[gi].color;
        g.globalAlpha = gi === 0 || gi === 1 ? 0.45 : 0.7;
        g.lineWidth = 1.5;
        for (const r of this.byLane[li]) {
          const yy = Math.round(y0 + 3 + hash(r[3]) * (h - 6)) + 0.5;
          g.beginPath();
          g.moveTo(x(r[0]), yy);
          g.lineTo(Math.max(x(r[0] + r[1]), x(r[0]) + 1.5), yy);
          g.stroke();
        }
        g.globalAlpha = 1;
      });
      // Landed on main, per hour
      g.fillStyle = muted;
      g.fillText("landed", 4, L.landedY + 22);
      const maxL = Math.max(1, ...d.hourly.map((h) => h.l));
      g.fillStyle = ink;
      d.hourly.forEach((h, hi) => {
        if (!h.l) return;
        const bh = (h.l / maxL) * 30;
        const xx = x(hi * 60);
        g.fillRect(xx, L.landedY + 34 - bh, Math.max(1, x(60) - x(0) - 0.5), bh);
      });
    }
    hover(e) {
      const d = this.d, L = this.L;
      const rect = this.canvas.getBoundingClientRect();
      const px = e.clientX - rect.left, py = e.clientY - rect.top;
      const li = L.ys.findIndex((y0, i) => py >= y0 - 2 && py <= y0 + L.heights[i] + 2);
      const m = ((px - L.left) / (L.W - L.left - L.right)) * this.span;
      if (py >= L.landedY && px > L.left) {
        const hi = Math.floor(m / 60);
        const h = d.hourly[hi];
        if (h) return showTip(e, [{ head: true, label: stamp(at(d, hi * 60)) }, { value: num(h.l), label: "changes landed on main this hour" }]);
      }
      if (li < 0 || px < L.left) return hideTip();
      const tol = (6 / (L.W - L.left - L.right)) * this.span;
      let best = null, bestD = Infinity;
      for (const r of this.byLane[li]) {
        if (m < r[0] - tol || m > r[0] + r[1] + tol) continue;
        const yy = L.ys[li] + 3 + hash(r[3]) * (L.heights[li] - 6);
        const dy = Math.abs(yy - py);
        if (dy < bestD) {
          bestD = dy;
          best = r;
        }
      }
      if (!best || bestD > 6) return hideTip();
      const [title, role, model] = d.titles[best[3]] ?? ["", "", ""];
      const gi = groupOfRole[role];
      showTip(e, [
        { head: true, label: title },
        { color: GROUPS[gi].color, value: `${GROUPS[gi].label.split(",")[0]}`, label: `${MODEL_LABEL[model] ?? model} · ${best[4] >= 0 ? WHY[best[4]] : ""}` },
        { value: dur(best[1]), label: `from ${stamp(at(d, best[0]))}` },
      ]);
    }
  }

  // ---------------------------------------------------------------------------
  // Work over time: agents at work, runs started, changes landed

  class Activity extends Chart {
    setup(d) {
      this.metric = "a";
      this.res = "hour";
      const bar = html("div", { class: "fx-controls" }, this);
      segmented(bar, [["a", "Agents at work"], ["s", "Runs started"], ["l", "Landed on main"]], "a", (v) => ((this.metric = v), this.draw()), "Metric");
      segmented(bar, [["hour", "By hour"], ["day", "By day"]], "hour", (v) => ((this.res = v), this.draw()), "Resolution");
      this.box = html("div", { class: "fx-box" }, this);
      this.leg = html("div", {}, this);
      const days = this.series("day", "a");
      table(
        this,
        "Daily totals",
        ["Day", "Agent-hours", "Runs started", "Landed"],
        days.labels.map((l, i) => [l, num(days.totals[i] * 24), num(this.series("day", "s").totals[i]), num(this.series("day", "l").totals[i])]),
      );
    }
    series(res, metric) {
      const d = this.d;
      const hours = d.hourly;
      const values = (h) =>
        metric === "l" ? [h.l] : GROUPS.map((g) => g.roles.reduce((n, r) => n + (h[metric][r] ?? 0), 0));
      let rows = hours.map(values), labels = hours.map((_, i) => stamp(at(d, i * 60)));
      if (res === "day") {
        const byDay = new Map();
        hours.forEach((h, i) => {
          const key = at(d, i * 60).toISOString().slice(0, 10);
          const v = values(h);
          const acc = byDay.get(key) ?? v.map(() => 0);
          v.forEach((x, j) => (acc[j] += metric === "a" ? x / 24 : x));
          byDay.set(key, acc);
        });
        labels = [...byDay.keys()].map((k) => dayLabel(new Date(`${k}T00:00:00Z`)));
        rows = [...byDay.values()];
      }
      return { rows, labels, totals: rows.map((r) => r.reduce((a, b) => a + b, 0)) };
    }
    draw() {
      const d = this.d;
      const { rows, labels, totals } = this.series(this.res, this.metric);
      const single = this.metric === "l";
      const W = this.clientWidth, H = 260, m = { l: 36, r: 8, t: 14, b: 24 };
      this.box.replaceChildren();
      const s = svg("svg", { width: W, height: H, viewBox: `0 0 ${W} ${H}`, role: "img", "aria-label": "Agent activity over time" }, this.box);
      const max = Math.max(...totals) * 1.08 || 1;
      const n = rows.length;
      const X = (i) => m.l + (i / Math.max(1, n - 1)) * (W - m.l - m.r);
      const bw = (W - m.l - m.r) / n;
      const Y = (v) => H - m.b - (v / max) * (H - m.t - m.b);
      // grid
      const step = niceStep(max / 4);
      for (let v = 0; v <= max; v += step) {
        svg("line", { x1: m.l, x2: W - m.r, y1: Y(v), y2: Y(v), class: "fx-grid" }, s);
        svg("text", { x: m.l - 6, y: Y(v) + 4, class: "fx-axis", "text-anchor": "end" }, s).textContent = big(v);
      }
      const every = Math.ceil(n / (W < 520 ? 4 : 7));
      labels.forEach((lab, i) => {
        if (i % every) return;
        svg("text", { x: single ? m.l + bw * (i + 0.5) : X(i), y: H - 6, class: "fx-axis", "text-anchor": "middle" }, s).textContent =
          this.res === "hour" ? lab.split(",")[0] : lab;
      });
      if (single) {
        rows.forEach((r, i) => {
          if (!r[0]) return;
          svg("rect", { x: m.l + bw * i + 0.5, y: Y(r[0]), width: Math.max(1, bw - 1.5), height: H - m.b - Y(r[0]), fill: GROUPS[2].color, rx: Math.min(2, bw / 3) }, s);
        });
      } else {
        // Stacked areas in group order, a surface-colored stroke between fills.
        let base = rows.map(() => 0);
        GROUPS.forEach((g, gi) => {
          const top = rows.map((r, i) => base[i] + r[gi]);
          if (top.every((v, i) => v === base[i])) return;
          let p = `M${X(0)},${Y(top[0])}`;
          for (let i = 1; i < n; i++) p += `L${X(i)},${Y(top[i])}`;
          for (let i = n - 1; i >= 0; i--) p += `L${X(i)},${Y(base[i])}`;
          svg("path", { d: `${p}Z`, fill: g.color, class: "fx-area" }, s);
          base = top;
        });
      }
      svg("line", { x1: m.l, x2: W - m.r, y1: H - m.b, y2: H - m.b, class: "fx-base" }, s);
      // Peak label
      const peak = totals.indexOf(Math.max(...totals));
      const px = single ? m.l + bw * (peak + 0.5) : X(peak);
      const unit = this.metric === "a" ? "agents at once" : this.metric === "s" ? "runs started" : "landed";
      const nearRight = px > W - 170;
      const lbl = svg("text", { x: nearRight ? px - 6 : px + 6, y: Math.max(m.t + 10, Y(totals[peak]) + 4), class: "fx-note", "text-anchor": nearRight ? "end" : "start" }, s);
      lbl.textContent = `peak ${this.metric === "a" ? totals[peak].toFixed(1) : num(totals[peak])} ${unit}`;
      // Crosshair
      const cross = svg("line", { y1: m.t, y2: H - m.b, class: "fx-cross", visibility: "hidden" }, s);
      const hit = svg("rect", { x: m.l, y: 0, width: W - m.l - m.r, height: H, fill: "transparent" }, s);
      hit.addEventListener("pointermove", (e) => {
        const r = s.getBoundingClientRect();
        const i = Math.max(0, Math.min(n - 1, Math.round(((e.clientX - r.left - m.l) / (W - m.l - m.r)) * (n - 1))));
        const xx = single ? m.l + bw * (i + 0.5) : X(i);
        cross.setAttribute("x1", xx);
        cross.setAttribute("x2", xx);
        cross.setAttribute("visibility", "visible");
        const fmt = (v) => (this.metric === "a" ? v.toFixed(1) : num(v));
        showTip(e, [
          { head: true, label: labels[i] },
          { value: fmt(totals[i]), label: single ? "changes landed" : this.metric === "a" ? "agents at work (average)" : "runs started" },
          ...(single ? [] : GROUPS.map((g, gi) => ({ color: g.color, value: fmt(rows[i][gi]), label: g.label })).filter((r, gi) => rows[i][gi] > 0)),
        ]);
      });
      hit.addEventListener("pointerleave", () => {
        cross.setAttribute("visibility", "hidden");
        hideTip();
      });
      this.leg.replaceChildren();
      legend(this.leg, single ? [{ label: "changes landed on main", color: GROUPS[2].color }] : GROUPS.map((g) => ({ label: g.label, color: g.color })));
    }
  }
  function niceStep(x) {
    const p = 10 ** Math.floor(Math.log10(x || 1));
    for (const f of [1, 2, 2.5, 5, 10]) if (f * p >= x) return f * p;
    return 10 * p;
  }

  // ---------------------------------------------------------------------------
  // Models: who did the work, at what token cost

  class Models extends Chart {
    setup(d) {
      this.metric = "total";
      const bar = html("div", { class: "fx-controls" }, this);
      segmented(bar, [["total", "Tokens"], ["hours", "Agent-hours"], ["tasks", "Agents"]], "total", (v) => ((this.metric = v), this.draw()), "Metric");
      this.box = html("div", { class: "fx-bars" }, this);
      const t = d.totals;
      const comp = html("div", { class: "fx-comp" }, this);
      html("div", { class: "fx-comp-title" }, comp, "Where the tokens go");
      const strip = html("div", { class: "fx-strip" }, comp);
      const parts = [
        ["cached reads", t.cacheTokens, "var(--ink-soft)"],
        ["fresh input", t.inputTokens, "var(--muted)"],
        ["output", t.outputTokens, "var(--ink)"],
      ];
      for (const [label, v, color] of parts) {
        const seg = html("span", { style: `flex:${Math.max(v / t.tokens, 0.004)};background:${color}` }, strip);
        seg.addEventListener("pointermove", (e) => showTip(e, [{ value: big(v), label }, { label: `${((v / t.tokens) * 100).toFixed(1)}% of all tokens` }]));
        seg.addEventListener("pointerleave", hideTip);
      }
      legend(comp, parts.map(([label, v, color]) => ({ label: `${label} ${((v / t.tokens) * 100).toFixed(1)}%`, color })));
      table(
        this,
        "Per model",
        ["Model", "Agents", "Runs", "Agent-hours", "Input", "Output", "Cached reads", "Total"],
        d.models.map((m) => [MODEL_LABEL[m.model] ?? m.model, num(m.tasks), num(m.runs), num(m.hours), big(m.input), big(m.output), big(m.cache), big(m.total)]),
      );
    }
    draw() {
      const d = this.d;
      this.box.replaceChildren();
      const max = Math.max(...d.models.map((m) => m[this.metric]));
      for (const m of d.models) {
        const row = html("div", { class: "fx-bar-row" }, this.box);
        const head = html("div", { class: "fx-bar-label" }, row);
        html("strong", {}, head, MODEL_LABEL[m.model] ?? m.model);
        const roles = Object.entries(m.roles).sort((a, b) => b[1] - a[1]).map(([r]) => GROUPS[groupOfRole[r]]?.label.split(",")[0].toLowerCase() ?? r);
        html("small", {}, head, [...new Set(roles)].join(", "));
        const track = html("div", { class: "fx-bar-track" }, row);
        const v = m[this.metric];
        const fill = html("span", { class: "fx-bar-fill", style: `width:${(v / max) * 100}%` }, track);
        html("span", { class: "fx-bar-value" }, row, this.metric === "total" ? big(v) : num(v));
        fill.addEventListener("pointermove", (e) =>
          showTip(e, [
            { head: true, label: MODEL_LABEL[m.model] ?? m.model },
            { value: big(m.total), label: "tokens" },
            { value: num(m.hours), label: "agent-hours" },
            { value: num(m.tasks), label: `agents, ${num(m.runs)} runs` },
          ]),
        );
        fill.addEventListener("pointerleave", hideTip);
      }
    }
  }

  // ---------------------------------------------------------------------------
  // How work lands: a flow from first review to who landed it

  class Flow extends Chart {
    setup(d) {
      this.box = html("div", { class: "fx-box" }, this);
      const f = d.flow;
      const rounds = html("div", { class: "fx-rounds" }, this);
      html("div", { class: "fx-comp-title" }, rounds, "Review rounds before a change landed");
      const maxR = Math.max(...Object.values(f.rounds));
      const cols = html("div", { class: "fx-cols" }, rounds);
      for (const [r, n] of Object.entries(f.rounds)) {
        const c = html("div", { class: "fx-col" }, cols);
        html("span", { class: "fx-col-value" }, c, num(n));
        html("span", { class: "fx-col-bar", style: `height:${(n / maxR) * 72}px` }, c);
        html("span", { class: "fx-col-label" }, c, r === "5" ? "5+" : r);
      }
      table(
        this,
        "Every path",
        ["First review", "Outcome", "Landed by", "Changes"],
        Object.entries(f.paths).sort((a, b) => b[1] - a[1]).map(([k, v]) => [...k.split("|").map((x) => x || "—"), num(v)]),
      );
    }
    draw() {
      const f = this.d.flow;
      const W = this.clientWidth, H = W < 520 ? 300 : 330;
      this.box.replaceChildren();
      const s = svg("svg", { width: W, height: H, viewBox: `0 0 ${W} ${H}`, role: "img", "aria-label": "How code work flows from first review to landing" }, this.box);
      const cols = [
        [{ id: "all", label: "Code work" }],
        [{ id: "approved", label: "Approved first time" }, { id: "changes requested", label: "Changes requested" }, { id: "not reviewed", label: "Never reviewed" }],
        [{ id: "landed", label: "Landed on main" }, { id: "discarded", label: "Discarded" }, { id: "in flight", label: "Still in flight" }],
        [{ id: "auto", label: "Automatically" }, { id: "maintainer", label: "By the maintainer" }, { id: "integration agent", label: "By the agent" }],
      ];
      const paths = Object.entries(f.paths).map(([k, v]) => {
        const [first, outcome, how] = k.split("|");
        return { first, outcome, how, v };
      });
      const total = paths.reduce((n, p) => n + p.v, 0);
      const value = (ci, id) =>
        ci === 0 ? total : paths.filter((p) => (ci === 1 ? p.first : ci === 2 ? p.outcome : p.how) === id).reduce((n, p) => n + p.v, 0);
      const nodeW = 10, gap = 18, top = 8, bottom = 8;
      const narrow = W < 520;
      const labelRoom = narrow ? 92 : 140;
      const short = { auto: "auto", maintainer: "maintainer", "integration agent": "agent" };
      const usableW = W - labelRoom - nodeW;
      const colX = cols.map((_, ci) => (ci / (cols.length - 1)) * usableW);
      const scale = (H - top - bottom - gap * 2) / total;
      const nodes = {};
      cols.forEach((col, ci) => {
        let y = top;
        for (const n of col) {
          const v = value(ci, n.id);
          if (!v) continue;
          const h = Math.max(2, v * scale);
          nodes[`${ci}:${n.id}`] = { x: colX[ci], y, h, v, label: n.label, inY: y, outY: y };
          y += h + gap;
        }
      });
      const color = { landed: GROUPS[2].color, discarded: "var(--muted)", "in flight": "var(--ink-soft)" };
      const link = (a, b, v, c, label) => {
        const h = v * scale;
        const x0 = a.x + nodeW, x1 = b.x, y0 = a.outY, y1 = b.inY;
        a.outY += h;
        b.inY += h;
        const mx = (x0 + x1) / 2;
        const p = svg("path", {
          d: `M${x0},${y0}C${mx},${y0} ${mx},${y1} ${x1},${y1}L${x1},${y1 + h}C${mx},${y1 + h} ${mx},${y0 + h} ${x0},${y0 + h}Z`,
          fill: c,
          class: "fx-link",
        }, s);
        p.addEventListener("pointermove", (e) => showTip(e, [{ value: num(v), label }, { label: `${((v / total) * 100).toFixed(0)}% of all code work` }]));
        p.addEventListener("pointerleave", hideTip);
      };
      // links in a stable order so bands don't cross needlessly
      for (const n of cols[1]) {
        const v = value(1, n.id);
        if (v) link(nodes["0:all"], nodes[`1:${n.id}`], v, "var(--ink-soft)", `${n.label.toLowerCase()}`);
      }
      for (const a of cols[1]) for (const b of cols[2]) {
        const v = paths.filter((p) => p.first === a.id && p.outcome === b.id).reduce((n, p) => n + p.v, 0);
        if (v) link(nodes[`1:${a.id}`], nodes[`2:${b.id}`], v, color[b.id], `${a.label.toLowerCase()}, then ${b.label.toLowerCase()}`);
      }
      for (const b of cols[3]) {
        const v = value(3, b.id);
        if (v) link(nodes["2:landed"], nodes[`3:${b.id}`], v, color.landed, `landed: ${b.label.toLowerCase()}`);
      }
      for (const [key, n] of Object.entries(nodes)) {
        const ci = Number(key[0]);
        svg("rect", { x: n.x, y: n.y, width: nodeW, height: n.h, rx: 2, class: "fx-node" }, s);
        const right = ci < cols.length - 1 || labelRoom > 0;
        const tx = ci === cols.length - 1 ? n.x + nodeW + 6 : n.x + nodeW + 6;
        const t = svg("text", { x: tx, y: n.y + Math.min(n.h, 30) / 2 + 4, class: "fx-flow-label", "text-anchor": right ? "start" : "end" }, s);
        const strong = svg("tspan", { class: "fx-flow-value" }, t);
        strong.textContent = `${num(n.v)} `;
        const rest = svg("tspan", {}, t);
        rest.textContent = !narrow ? n.label.toLowerCase() : ci === 3 ? short[key.slice(2)] ?? "" : "";
      }
    }
  }

  // ---------------------------------------------------------------------------
  // Anatomy of one roadmap item: every agent, every run, every handover

  class Anatomy extends Chart {
    setup(d) {
      this.plan = d.plans[0];
      const bar = html("div", { class: "fx-controls" }, this);
      const sel = html("label", { class: "fx-select" }, bar);
      sel.appendChild(document.createTextNode("Roadmap item "));
      const select = html("select", { id: "fx-plan" }, sel);
      d.plans.forEach((p, i) => html("option", { value: String(i) }, select, p.title));
      select.addEventListener("change", () => {
        this.plan = d.plans[Number(select.value)];
        this.draw();
      });
      this.summary = html("p", { class: "fx-summary" }, this);
      this.box = html("div", { class: "fx-box fx-scroll" }, this);
      legend(this, [
        { label: "Planner", color: GROUPS[6].color },
        { label: "○ plan approved", color: "transparent" },
        { label: "Implementer: first build", color: GROUPS[1].color },
        { label: "Implementer: fixing review feedback", color: "url(#fx-hatch)" },
        { label: "Reviewer", color: GROUPS[0].color },
        { label: "Landing on main", color: GROUPS[2].color },
        { label: "◆ landed", color: "transparent" },
        { label: "→ stacked on", color: "transparent" },
      ]).querySelectorAll(".fx-swatch").forEach((sw, i) => {
        if (i === 1) sw.remove();
        if (i === 3) sw.style.background = `repeating-linear-gradient(135deg, ${GROUPS[1].color} 0 3px, #f4f1ea 3px 5px)`;
        if (i >= 6) sw.remove();
      });
      this.tableBox = html("div", {}, this);
    }
    draw() {
      const p = this.plan;
      const d = this.d;
      const tasks = p.tasks;
      const steps = tasks.filter((t) => t.kind === "plan_step").sort((a, b) => a.id - b.id);
      const planTask = tasks.find((t) => t.kind === "plan");
      const childrenOf = (id) => tasks.filter((t) => t.parent === id && (t.kind === "review" || t.kind === "integrate"));
      const rows = [{ task: planTask, label: "Plan", subs: [] }, ...steps.map((s, i) => ({ task: s, label: `${i + 1}. ${s.title}`, subs: childrenOf(s.id) }))];
      const allRuns = tasks.flatMap((t) => t.runs);
      const t0 = Math.min(p.start, ...allRuns.map((r) => r[0]));
      const t1 = Math.max(...allRuns.map((r) => r[0] + r[1]), ...steps.map((s) => s.landed ?? 0));
      const W = Math.max(this.clientWidth, 560);
      const labelW = this.clientWidth < 520 ? 130 : 210, right = 16, rowH = 30, top = 26;
      const H = top + rows.length * rowH + 10;
      const X = (m) => labelW + ((m - t0) / (t1 - t0)) * (W - labelW - right);
      this.box.replaceChildren();
      const s = svg("svg", { width: W, height: H, viewBox: `0 0 ${W} ${H}`, role: "img", "aria-label": `Every agent run for ${p.title}` }, this.box);
      const defs = svg("defs", {}, s);
      const pat = svg("pattern", { id: "fx-hatch", width: 5, height: 5, patternUnits: "userSpaceOnUse", patternTransform: "rotate(45)" }, defs);
      svg("rect", { width: 5, height: 5, fill: "#f4f1ea" }, pat);
      svg("rect", { width: 3, height: 5, fill: GROUPS[1].color }, pat);
      const mk = svg("marker", { id: "fx-arrow", viewBox: "0 0 6 6", refX: 5, refY: 3, markerWidth: 6, markerHeight: 6, orient: "auto" }, defs);
      svg("path", { d: "M0,0L6,3L0,6Z", class: "fx-arrowhead" }, mk);
      // hour grid
      const spanH = (t1 - t0) / 60;
      const stepH = spanH > 72 ? 24 : spanH > 30 ? 6 : spanH > 12 ? 3 : 1;
      svg("text", { x: 4, y: 12, class: "fx-axis" }, s).textContent = `from ${stamp(at(d, t0))}`;
      for (let hh = 0; hh <= spanH; hh += stepH) {
        const xx = X(t0 + hh * 60);
        svg("line", { x1: xx, x2: xx, y1: top - 6, y2: H - 6, class: "fx-grid" }, s);
        if (hh > 0) svg("text", { x: xx + 3, y: 12, class: "fx-axis" }, s).textContent = stepH >= 24 ? `day ${hh / 24 + 1}` : `+${hh}h`;
      }
      const rowY = (i) => top + i * rowH;
      const hover = (el, rowsOf) => {
        el.addEventListener("pointermove", (e) => showTip(e, rowsOf()));
        el.addEventListener("pointerleave", hideTip);
      };
      const segment = (t, r, y, h, fill, extraClass) => {
        const rect = svg("rect", { x: X(r[0]), y, width: Math.max(2, X(r[0] + r[1]) - X(r[0])), height: h, rx: 2, fill, class: `fx-seg-run ${extraClass ?? ""}` }, s);
        hover(rect, () => [
          { head: true, label: t.title },
          { color: GROUPS[groupOfRole[t.role]].color, value: GROUPS[groupOfRole[t.role]].label.split(",")[0], label: `${MODEL_LABEL[t.model] ?? t.model ?? ""}${t.kind === "review" && t.verdict ? ` · ${t.verdict === "approve" ? "approved" : "requested changes"}` : ""}` },
          { value: dur(r[1]), label: `${r[2] === "start" ? "" : `${WHY[["start", "fix", "land", "push", "steer", "retry"].indexOf(r[2])]}, `}from ${stamp(at(d, r[0]))}` },
        ]);
      };
      // The plan waits for the maintainer: its steps are created the moment it's approved.
      const planEnd = planTask.runs.length ? Math.max(...planTask.runs.map((r) => r[0] + r[1])) : p.start;
      const approved = steps.length ? Math.min(...steps.map((x) => x.created ?? Infinity)) : null;
      if (approved != null && Number.isFinite(approved) && approved > planEnd) {
        const y = rowY(0) + rowH / 2;
        const wait = svg("line", { x1: X(planEnd), x2: X(approved), y1: y, y2: y, class: "fx-wait" }, s);
        const ok = svg("circle", { cx: X(approved), cy: y, r: 4, class: "fx-approved" }, s);
        const waitRows = () => [
          { head: true, label: "Waiting for the maintainer" },
          { value: dur(approved - planEnd), label: `from the plan being submitted to approval, ${stamp(at(d, approved))}` },
        ];
        hover(wait, waitRows);
        hover(ok, waitRows);
        if (X(approved) - X(planEnd) > 150) {
          svg("text", { x: (X(planEnd) + X(approved)) / 2, y: y - 5, class: "fx-axis", "text-anchor": "middle" }, s).textContent = "waiting for approval";
        }
      }
      const readyAt = new Map();
      rows.forEach((row, i) => {
        const y = rowY(i);
        if (i % 2 === 0) svg("rect", { x: 0, y, width: W, height: rowH, class: "fx-band" }, s);
        const lab = svg("text", { x: 4, y: y + rowH / 2 + 4, class: "fx-row-label" }, s);
        const max = Math.floor(labelW / 7.2);
        lab.textContent = row.label.length > max ? `${row.label.slice(0, max - 1)}…` : row.label;
        hover(lab, () => [{ head: true, label: row.task.title }, { label: `${row.subs.filter((x) => x.kind === "review").length} reviews · ${row.task.runs.length} runs by the author` }]);
        for (const r of row.task.runs) {
          const fill = row.task.kind === "plan" ? GROUPS[6].color : r[2] === "fix" ? "url(#fx-hatch)" : r[2] === "land" ? GROUPS[2].color : GROUPS[1].color;
          segment(row.task, r, y + 4, 12, fill, r[2] === "fix" ? "fx-hatched" : "");
        }
        for (const sub of row.subs) for (const r of sub.runs) segment(sub, r, y + 18, 8, sub.kind === "integrate" ? GROUPS[2].color : GROUPS[0].color);
        if (row.task.landed != null) {
          const xx = X(row.task.landed);
          const dia = svg("path", { d: `M${xx},${y + 3}L${xx + 6},${y + 10}L${xx},${y + 17}L${xx - 6},${y + 10}Z`, class: "fx-landed" }, s);
          hover(dia, () => [{ head: true, label: row.task.title }, { value: "Landed on main", label: stamp(at(d, row.task.landed)) }]);
        }
        // When this step was ready to stack on: its first approving review ended.
        const ok = row.subs.filter((x) => x.kind === "review" && x.verdict === "approve").flatMap((x) => x.runs.map((r) => r[0] + r[1]));
        readyAt.set(row.task.id, { y, at: ok.length ? Math.min(...ok) : row.task.landed });
      });
      // Stacking arrows: from the dependency's approval to the dependent's first run
      rows.forEach((row, i) => {
        if (!row.task.deps?.length || !row.task.runs.length) return;
        const start = row.task.runs[0][0];
        for (const dep of row.task.deps) {
          const from = readyAt.get(dep);
          if (!from || from.at == null) continue;
          const x0 = X(Math.min(from.at, start)), y0 = from.y + 26, x1 = X(start), y1 = rowY(i) + 10;
          svg("path", { d: `M${x0},${y0}C${x0},${(y0 + y1) / 2} ${x1 - 14},${y1} ${x1 - 2},${y1}`, class: "fx-dep", "marker-end": "url(#fx-arrow)" }, s);
        }
      });
      const reviews = tasks.filter((t) => t.kind === "review");
      const integrators = tasks.filter((t) => t.kind === "integrate");
      const runs = tasks.reduce((n, t) => n + t.runs.length, 0);
      const landed = steps.filter((x) => x.landed != null).length;
      const stacked = steps.filter((x) => x.deps.length).length;
      const fixes = tasks.reduce((n, t) => n + t.runs.filter((r) => r[2] === "fix").length, 0);
      const handovers = reviews.length + fixes + steps.filter((x) => x.runs.some((r) => r[2] === "land")).length + integrators.length;
      const waited = approved != null && Number.isFinite(approved) ? Math.max(0, approved - planEnd) : 0;
      this.summary.textContent = `${tasks.length} agents, ${runs} runs and ${handovers} handovers: ${steps.length} steps, ${reviews.length} reviews (${reviews.filter((r) => r.verdict === "approve").length} approvals), ${fixes} rounds of fixes, ${landed} steps landed, ${stacked} of them stacked on another step's branch. ${((Math.max(...steps.map((x) => x.landed ?? 0)) - p.start) / 60).toFixed(1)} hours from the planner starting to the last step landing, ${dur(waited)} of it waiting for the plan to be approved.`;
      this.tableBox.replaceChildren();
      table(
        this.tableBox,
        "Every step",
        ["Step", "Stacked on", "Author runs", "Reviews", "Landed"],
        steps.map((x, i) => [
          `${i + 1}. ${x.title}`,
          x.deps.map((dep) => `${steps.findIndex((y) => y.id === dep) + 1}`).filter((v) => v !== "0").join(", ") || "—",
          String(x.runs.length),
          String(tasks.filter((t) => t.parent === x.id && t.kind === "review").length),
          x.landed != null ? stamp(at(d, x.landed)) : "—",
        ]),
      );
    }
  }

  // ---------------------------------------------------------------------------
  // Stack depth, and who made the calls

  class Stacks extends Chart {
    setup(d) {
      const hist = Object.entries(d.depthHist).map(([k, v]) => [Number(k), v]).sort((a, b) => a[0] - b[0]);
      const max = Math.max(...hist.map((h) => h[1]));
      html("div", { class: "fx-comp-title" }, this, "Plan steps by how deep in a stack they were built");
      const cols = html("div", { class: "fx-cols" }, this);
      for (const [depth, n] of hist) {
        const c = html("div", { class: "fx-col" }, cols);
        html("span", { class: "fx-col-value" }, c, num(n));
        const b = html("span", { class: "fx-col-bar", style: `height:${(n / max) * 90}px;background:${GROUPS[1].color}` }, c);
        b.addEventListener("pointermove", (e) =>
          showTip(e, [{ value: num(n), label: depth === 1 ? "steps built straight on main" : `steps built on a stack of ${depth - 1} unmerged branch${depth > 2 ? "es" : ""}` }]),
        );
        b.addEventListener("pointerleave", hideTip);
        html("span", { class: "fx-col-label" }, c, depth === 1 ? "main" : `+${depth - 1}`);
      }
      table(this, "Stack depth", ["Branches underneath", "Steps"], hist.map(([k, v]) => [String(k - 1), num(v)]));
    }
  }

  class Decisions extends Chart {
    setup(d) {
      const by = d.decisions.by;
      const parts = [
        ["maintainer", "The maintainer", "var(--ink)"],
        ["integration agent", "Integration agent", GROUPS[2].color],
        ["inbox agent", "Inbox agent", GROUPS[6].color],
        ["dismissed", "Dismissed (resolved on its own)", "var(--ink-soft)"],
      ].filter(([k]) => by[k]);
      const total = parts.reduce((n, [k]) => n + by[k], 0);
      html("div", { class: "fx-comp-title" }, this, `${num(d.decisions.total)} decisions the agents asked for`);
      const strip = html("div", { class: "fx-strip tall" }, this);
      for (const [k, label, color] of parts) {
        const seg = html("span", { style: `flex:${by[k] / total};background:${color}` }, strip);
        seg.addEventListener("pointermove", (e) => showTip(e, [{ value: num(by[k]), label }, { label: `${((by[k] / total) * 100).toFixed(0)}% of decisions` }]));
        seg.addEventListener("pointerleave", hideTip);
      }
      legend(this, parts.map(([k, label, color]) => ({ label: `${label} ${num(by[k])}`, color })));
      const kindLabel = { merge: "integrate this?", stuck: "stuck: retry or cancel?", approve_plan: "approve this plan?", start_fix: "start a fix?", question: "an agent's question", release_report: "release check report", approve_idea: "approve this idea?", start_task: "start this task?" };
      table(this, "By kind", ["Decision", "Count"], Object.entries(d.decisions.byKind).sort((a, b) => b[1] - a[1]).map(([k, v]) => [kindLabel[k] ?? k, num(v)]));
    }
  }

  const define = (name, cls) => customElements.get(name) || customElements.define(name, cls);
  define("factory-stats", Stats);
  define("factory-heartbeat", Heartbeat);
  define("factory-activity", Activity);
  define("factory-models", Models);
  define("factory-flow", Flow);
  define("factory-anatomy", Anatomy);
  define("factory-stacks", Stacks);
  define("factory-decisions", Decisions);
})();
