// The metrics charts: one or more graphs, each drawing its own metric over a
// shared time range with uPlot (served next to this file), refreshed every
// 5 seconds.
//
// Served by smweb, both on its own page and as a fragment that another web
// app embeds. The page says where to fetch from (data-url), so the same
// script works wherever smweb is mounted.
//
// The graphs are panels on a grid of rows, each row split into 6 columns.
// A panel is 2 to 6 columns wide, a row holds at most 3 panels and is always
// full (removing a panel widens its neighbour). Panels are moved by dragging
// their grip and resized by dragging the edge they share with their
// neighbour. The layout (rows, widths, the metric in each panel, and the
// "Last" range) is kept in a cookie, so the next visit shows the same thing. Dragging across a graph
// selects a period: every graph then shows exactly that period, "Last" is
// greyed out, and Reset goes back to the "Last" setting.
//
// The server does all the summarising: the data URL returns at most 600
// points however long the period, each the average of its time bucket, with
// the smallest and largest value in the bucket alongside. The line is the
// average and the shaded band is min-to-max, so a short spike is still
// visible on a week-long chart. A gap (the collector was down, or a value
// couldn't be measured) is a null and is drawn as a break in the line.
(function () {
  'use strict';

  const REFRESH_MS = 5000;
  const COOKIE = 'sm_layout';
  const root = document.getElementById('metrics-root');
  const rangeSelect = document.getElementById('range-select');
  const resetButton = document.getElementById('range-reset');
  const note = document.getElementById('selection-note');
  const updated = document.getElementById('updated');
  const graphsBox = document.getElementById('graphs');
  const addButton = document.getElementById('add-graph');
  const template = document.getElementById('graph-template');
  // The page has no controls if the collector was unreachable when it loaded.
  if (!root || !rangeSelect || !graphsBox || !template) return;

  // The time under the pointer is the same on every graph, so it is shown once,
  // in a bar below the panels (not in each legend). On smweb's own page (which
  // marks its body) the bar is docked at the foot of the window instead; see
  // page.css, which also gets a spacer so the last panels aren't covered.
  const dock = document.createElement('div');
  dock.id = 'time-dock';
  dock.textContent = '\u00a0';
  addButton.after(dock);
  if (document.body.hasAttribute('data-time-dock')) {
    const spacer = document.createElement('div');
    spacer.className = 'dock-spacer';
    dock.after(spacer);
  }

  const COLUMNS = 6, MIN_W = 2, MAX_PER_ROW = 3, MAX_PANELS = 20, GAP = 16;
  // The layout: rows of cells {g, w}; every row's widths add up to COLUMNS.
  let rows = [];
  const all = () => rows.flat().map(c => c.g);   // every panel (g: {el, select, box, ...})
  let selection = null;   // {from, to} in ms while a period is selected on a chart

  // ---- remembering the layout (a cookie; nice to have, never required) -------
  function remember() {
    try {
      const layout = {range: rangeSelect.value,
        rows: rows.map(r => r.map(c => [c.g.select.value, c.w]))};
      document.cookie = COOKIE + '=' + encodeURIComponent(JSON.stringify(layout)) +
        '; path=/; max-age=31536000; SameSite=Lax';
    } catch (e) { /* cookies may be blocked: carry on without them */ }
  }
  function saved() {
    try {
      const match = document.cookie.split('; ').find(c => c.startsWith(COOKIE + '='));
      if (match) return JSON.parse(decodeURIComponent(match.slice(COOKIE.length + 1))) || {};
    } catch (e) { /* unreadable: start afresh */ }
    return {};
  }

  // ---- formatting -----------------------------------------------------------
  const BINARY = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  function bytes(v) {
    let i = 0;
    let n = Math.abs(v);
    while (n >= 1024 && i < BINARY.length - 1) { n /= 1024; i++; }
    const sign = v < 0 ? '-' : '';
    return sign + (n >= 100 || i === 0 ? n.toFixed(0) : n.toFixed(1)) + ' ' + BINARY[i];
  }
  function formatter(unit) {
    switch (unit) {
      case 'bytes': return bytes;
      case 'bytes/s': return v => bytes(v) + '/s';
      case '%': return v => v.toFixed(1) + '%';
      case '°C': return v => v.toFixed(1) + ' °C';
      default: return v => v.toFixed(2);
    }
  }
  // Metrics that can't go below zero start their axis at zero, so a chart of
  // a mostly idle CPU doesn't look busy just because it is zoomed in.
  const NON_NEGATIVE = new Set(['bytes', 'bytes/s', '%', '']);
  function yRange(unit) {
    if (!NON_NEGATIVE.has(unit)) return undefined;
    return (u, min, max) => {
      const top = unit === '%' ? Math.min(Math.max(max * 1.1, 1), 100) : Math.max(max * 1.1, 1e-9);
      return [0, top];
    };
  }
  function clock(seconds) {
    return new Date(seconds * 1000).toLocaleTimeString([], {hour: '2-digit', minute: '2-digit', second: '2-digit'});
  }
  // 16.8333... minutes reads badly: one decimal place, none if it is whole.
  const tidy = n => String(Number(n.toFixed(1)));
  function duration(ms) {
    if (ms < 60000) return tidy(ms / 1000) + ' s';
    if (ms < 3600000) return tidy(ms / 60000) + ' min';
    return tidy(ms / 3600000) + ' h';
  }

  // ---- theme ------------------------------------------------------------------
  function css(name, fallback) {
    return getComputedStyle(document.documentElement).getPropertyValue(name).trim() || fallback;
  }

  // ---- one graph --------------------------------------------------------------
  function build(g, data) {
    if (g.plot) { g.plot.destroy(); g.plot = null; }
    const unit = data.metric.unit;
    const fmt = formatter(unit);
    const line = css('--pico-primary', '#0172ad');
    const text = css('--pico-muted-color', '#73818c');
    const grid = css('--pico-muted-border-color', '#e0e3e7');
    const shown = v => (v == null ? '-' : fmt(v));
    const xRange = selection ? () => [selection.from / 1000, selection.to / 1000] : undefined;
    const options = {
      width: Math.max(g.box.clientWidth, 120),
      height: 260,
      // Dragging across the chart selects a period (see the setSelect hook).
      // All graphs share the cursor: pointing at a time on one shows that
      // time (and its values) on every graph.
      cursor: {y: false, drag: {x: true, y: false, setScale: false}, sync: {key: 'sm', scales: ['x', null]}},
      select: {show: true},
      hooks: {
        // The graphs share the cursor, so the time under it is the same on all.
        setCursor: [u => {
          const i = u.cursor.idx;
          const t = i == null ? null : u.data[0][i];
          dock.textContent = t == null ? '\u00a0' : new Date(t * 1000).toLocaleString();
        }],
        setSelect: [u => {
          const w = u.select.width;
          if (w < 5) return;
          const from = Math.round(u.posToVal(u.select.left, 'x') * 1000);
          const to = Math.round(u.posToVal(u.select.left + w, 'x') * 1000);
          u.setSelect({left: 0, top: 0, width: 0, height: 0}, false);
          if (to - from >= 2000) select(from, to);
        }],
      },
      scales: {x: {time: true, range: xRange}, y: {range: yRange(unit)}},
      axes: [
        {stroke: text, grid: {stroke: grid, width: 1}, ticks: {stroke: grid}},
        {stroke: text, grid: {stroke: grid, width: 1}, ticks: {stroke: grid},
         size: 80, values: (u, splits) => splits.map(fmt)},
      ],
      series: [
        {},
        {label: 'average', stroke: line, width: 2, value: (u, v) => shown(v), spanGaps: false},
        {label: 'min', stroke: 'transparent', width: 0, value: (u, v) => shown(v), points: {show: false}},
        {label: 'max', stroke: 'transparent', width: 0, value: (u, v) => shown(v), points: {show: false}},
      ],
      // The shaded band between the smallest and the largest value per bucket.
      bands: [{series: [3, 2], fill: line + '33'}],
    };
    g.plot = new uPlot(options, columns(data), g.box);
  }

  // uPlot takes seconds, not milliseconds.
  function columns(data) {
    return [data.timestamps.map(t => t / 1000), data.avg, data.min, data.max];
  }

  // Per graph, only a problem is worth saying; "Updated" is shown once, top right.
  function describe(data) {
    return data.timestamps.length === 0 ? 'No data recorded for this period.' : '';
  }
  function updatedAt(data) {
    const points = data.timestamps.length;
    updated.textContent = 'Updated ' + clock(Date.now() / 1000);
    updated.title = points + ' points, ' + duration(data.step_ms) + ' each' +
      (points ? ', newest at ' + clock(data.timestamps[points - 1] / 1000) : '');
  }

  function fail(message) {
    updated.textContent = '';
    all().forEach(g => {
      g.status.textContent = message;
      g.status.classList.add('error');
    });
  }

  // One request fetches every graph's data: the server reads all the
  // metrics from the collector together.
  let inFlight = null;
  async function loadAll() {
    if (inFlight) inFlight.abort();
    const controller = new AbortController();
    inFlight = controller;
    const period = selection ? 'from=' + selection.from + '&to=' + selection.to
                             : 'range=' + encodeURIComponent(rangeSelect.value);
    const names = [...new Set(all().map(g => g.select.value))];
    const url = root.dataset.url + '?' +
      names.map(n => 'metric=' + encodeURIComponent(n)).join('&') + '&' + period;
    try {
      const response = await fetch(url, {signal: controller.signal, credentials: 'same-origin'});
      if (response.redirected || response.status === 303 || response.status === 401) {
        // A web app in front of this has sent us elsewhere (its login has
        // expired, say): reload, so it can send the whole page there.
        window.location.reload();
        return;
      }
      const reply = await response.json();
      if (!response.ok) {
        fail(reply.error || 'Could not load the metrics.');
        return;
      }
      all().forEach(g => {
        const one = reply.metrics[g.select.value];
        if (!one) return;
        const data = {
          metric: one.metric, step_ms: reply.step_ms, timestamps: reply.timestamps,
          avg: one.avg, min: one.min, max: one.max,
        };
        const key = g.select.value + '|' + period;
        g.status.classList.remove('error');
        if (g.plot && g.plotKey === key && !selection) {
          g.plot.setData(columns(data));   // same chart: just move the data along
        } else {
          build(g, data);
        }
        g.plotKey = key;
        g.box.setAttribute('aria-label', data.metric.label + (selection ? ' over the selected period'
          : ' over the last ' + rangeSelect.value));
        g.status.textContent = describe(data);
        updatedAt(data);
      });
    } catch (e) {
      if (e.name !== 'AbortError') fail('Could not load the metrics.');
    } finally {
      if (inFlight === controller) inFlight = null;
    }
  }

  // ---- the layout model ---------------------------------------------------------
  // These work on plain rows of cells {g, w} and never touch the page, so a
  // candidate layout can be worked out from the layout the drag started with.
  const cloneRows = m => m.map(r => r.map(c => ({g: c.g, w: c.w})));
  const signature = m => m.map(r => r.map(c => c.g.id + ':' + c.w).join(',')).join('|');
  const spread = r => r.forEach(c => { c.w = COLUMNS / r.length; });   // 1: 6, 2: 3+3, 3: 2+2+2
  const findRow = (m, g) => m.find(r => r.some(c => c.g === g));

  // Take a panel out of its row. Its neighbour takes up the space (the one
  // before it, or the one after it if it was first); an empty row goes.
  function detach(m, g) {
    const r = findRow(m, g);
    const i = r.findIndex(c => c.g === g);
    const [gone] = r.splice(i, 1);
    if (!r.length) m.splice(m.indexOf(r), 1);
    else r[Math.max(i - 1, 0)].w += gone.w;
    return gone;
  }

  // Where `g` ends up if dropped at `target`:
  //   {kind: 'in', anchor, before}  in the row holding `anchor`, ahead of the
  //                                 panel `before` (null: at the end);
  //   {kind: 'row', before}         on a row of its own, ahead of the row
  //                                 holding the panel `before` (null: last).
  // A full row can't take another panel: its last panel moves to a new row below.
  function place(base, g, target) {
    const m = cloneRows(base);
    const home = findRow(m, g);
    if (target.kind === 'in' && home && home.some(c => c.g === target.anchor)) {
      // Same row: just reorder, widths stay with their slots.
      const widths = home.map(c => c.w);
      const cell = home.splice(home.findIndex(c => c.g === g), 1)[0];
      const at = target.before ? home.findIndex(c => c.g === target.before) : home.length;
      home.splice(at, 0, cell);
      home.forEach((c, i) => { c.w = widths[i]; });
      return m;
    }
    detach(m, g);
    if (target.kind === 'row') {
      const at = target.before ? m.indexOf(findRow(m, target.before)) : m.length;
      m.splice(at, 0, [{g, w: COLUMNS}]);
      return m;
    }
    const row = findRow(m, target.anchor);
    const at = target.before ? row.findIndex(c => c.g === target.before) : row.length;
    row.splice(at, 0, {g, w: COLUMNS});
    if (row.length > MAX_PER_ROW) {
      const pushed = row.pop();
      pushed.w = COLUMNS;
      m.splice(m.indexOf(row) + 1, 0, [pushed]);
    }
    spread(row);
    return m;
  }

  // Which panels/rows a layout from the cookie may hold: anything odd (wrong
  // widths, too many in a row) is tidied rather than trusted.
  function fromSaved(layout) {
    let list = [];
    if (Array.isArray(layout.rows)) {
      list = layout.rows.filter(Array.isArray).map(r => r.map(x => Array.isArray(x) ? x : [x, 0]));
    } else if (Array.isArray(layout.metrics)) {
      list = layout.metrics.map(x => [[x, COLUMNS]]);   // the old layout: one graph per row
    }
    const m = [];
    let count = 0;
    list.forEach(r => {
      for (let i = 0; i < r.length && count < MAX_PANELS; i += MAX_PER_ROW) {
        const cells = r.slice(i, i + MAX_PER_ROW).map(([metric, w]) => ({g: addPanel(metric), w: Number(w)}));
        count += cells.length;
        const ok = cells.every(c => Number.isInteger(c.w) && c.w >= MIN_W) &&
          cells.reduce((t, c) => t + c.w, 0) === COLUMNS;
        if (!ok) spread(cells);
        if (cells.length) m.push(cells);
      }
    });
    return m;
  }

  // ---- showing the layout -------------------------------------------------------
  let shown = '';
  function render(force) {
    const sig = signature(rows);
    if (sig !== shown || force) {
      shown = sig;
      const els = rows.map(r => {
        const row = document.createElement('div');
        row.className = 'panel-row';
        r.forEach(c => row.appendChild(c.g.el));
        return row;
      });
      graphsBox.replaceChildren(...els);
    }
    rows.forEach(r => r.forEach((c, i) => {
      c.g.el.style.gridColumn = 'span ' + c.w;
      c.g.resizer.hidden = i === r.length - 1;
    }));
    const only = rows.length === 1 && rows[0].length === 1;
    all().forEach(g => { g.del.hidden = only; });
  }

  // Charts follow their panel's width as it changes.
  const watcher = new ResizeObserver(entries => entries.forEach(e => {
    const g = all().find(p => p.box === e.target);
    if (g && g.plot) g.plot.setSize({width: Math.max(g.box.clientWidth, 120), height: 260});
  }));

  // ---- one panel -----------------------------------------------------------------
  let nextId = 1;
  function addPanel(metric) {
    const el = template.content.firstElementChild.cloneNode(true);
    const g = {
      id: nextId++,
      el,
      select: el.querySelector('.metric-select'),
      box: el.querySelector('.chart'),
      status: el.querySelector('.chart-status'),
      del: el.querySelector('.graph-delete'),
      grip: el.querySelector('.graph-grip'),
      resizer: document.createElement('div'),
      plot: null, plotKey: null,
    };
    g.resizer.className = 'col-resizer';
    g.resizer.title = 'Drag to resize';
    el.appendChild(g.resizer);
    if (metric && [...g.select.options].some(o => o.value === metric)) g.select.value = metric;
    g.select.addEventListener('change', () => { remember(); loadAll(); });
    g.del.addEventListener('click', () => {
      if (all().length < 2) return;
      detach(rows, g);
      if (g.plot) g.plot.destroy();
      watcher.unobserve(g.box);
      el.remove();
      render();
      remember();
    });
    g.grip.addEventListener('pointerdown', e => startMove(e, g));
    g.resizer.addEventListener('pointerdown', e => startResize(e, g));
    watcher.observe(g.box);
    return g;
  }
  addButton.addEventListener('click', () => {
    // A new panel gets a row of its own at the bottom, starting as a copy of
    // the last one's metric; drag it where it should go and change it from there.
    if (all().length >= MAX_PANELS) return;
    const last = all().pop();
    const g = addPanel(last ? last.select.value : null);
    rows.push([{g, w: COLUMNS}]);
    render();
    remember();
    loadAll();
    g.el.scrollIntoView({block: 'nearest', behavior: 'smooth'});
  });

  // ---- resizing: drag the edge a panel shares with the next one -------------------
  function startResize(e, g) {
    if (e.button !== 0) return;
    e.preventDefault();
    const row = findRow(rows, g);
    const i = row.findIndex(c => c.g === g);
    const left = row[i], right = row[i + 1];
    const total = left.w + right.w;
    const unit = (graphsBox.getBoundingClientRect().width + GAP) / COLUMNS;   // one column and a gap
    const x0 = e.clientX, w0 = left.w;
    document.body.classList.add('panels-resizing');
    const move = ev => {
      left.w = Math.min(Math.max(w0 + Math.round((ev.clientX - x0) / unit), MIN_W), total - MIN_W);
      right.w = total - left.w;
      render();
    };
    const stop = () => {
      document.removeEventListener('pointermove', move);
      document.removeEventListener('pointerup', stop);
      document.removeEventListener('pointercancel', stop);
      document.body.classList.remove('panels-resizing');
      remember();
    };
    document.addEventListener('pointermove', move);
    document.addEventListener('pointerup', stop);
    document.addEventListener('pointercancel', stop);
  }

  // ---- moving: drag a panel's grip -------------------------------------------------
  // While dragging, a see-through copy follows the pointer, the panel itself
  // stays in the grid as a faint placeholder at the spot it would land, and
  // the other panels make room for it as you go.
  function startMove(e, g) {
    if (e.button !== 0) return;
    e.preventDefault();
    const base = cloneRows(rows);
    const rect = g.el.getBoundingClientRect();
    const ghost = document.createElement('div');
    ghost.className = 'panel-ghost';
    ghost.style.width = rect.width + 'px';
    ghost.style.height = rect.height + 'px';
    ghost.textContent = g.select.selectedOptions[0].textContent;
    document.body.appendChild(ghost);
    g.el.classList.add('dragging');
    document.body.classList.add('panels-dragging');
    const dx = e.clientX - rect.left, dy = e.clientY - rect.top;
    let x = e.clientX, y = e.clientY;

    // Where is the pointer, in terms of the layout now on show? The top and
    // bottom fifth of a row mean "a row of its own here"; the middle means
    // "into this row", at the place the pointer's x says.
    function targetAt() {
      const els = [...graphsBox.children];
      if (!els.length) return null;
      const others = r => r.filter(c => c.g !== g);
      const firstOther = from => { for (let i = from; i < rows.length; i++) { const o = others(rows[i]); if (o.length) return o[0].g; } return null; };
      let at = els.findIndex(el => y < el.getBoundingClientRect().bottom);
      if (at < 0) return {kind: 'row', before: null};
      const box = els[at].getBoundingClientRect();
      const frac = (y - box.top) / Math.max(box.height, 1);
      if (frac < 0 || frac < 0.2) return {kind: 'row', before: firstOther(at)};
      if (frac > 0.8) return {kind: 'row', before: firstOther(at + 1)};
      const mine = others(rows[at]);
      if (!mine.length) return null;   // already alone on its row
      const before = mine.findIndex(c => x < c.g.el.getBoundingClientRect().left + c.g.el.offsetWidth / 2);
      return {kind: 'in', anchor: mine[0].g, before: before < 0 ? null : mine[before].g};
    }
    function update() {
      ghost.style.transform = 'translate(' + (x - dx) + 'px,' + (y - dy) + 'px)';
      const target = targetAt();
      if (!target) return;
      const next = place(base, g, target);
      if (signature(next) !== signature(rows)) { rows = next; render(); }
    }
    // Scroll when the pointer is near the top or bottom of the window.
    const scroller = setInterval(() => {
      const edge = 60;
      const by = y < edge ? -18 : (y > window.innerHeight - edge ? 18 : 0);
      if (by) { window.scrollBy(0, by); update(); }
    }, 30);
    const move = ev => { x = ev.clientX; y = ev.clientY; update(); };
    const stop = ev => {
      document.removeEventListener('pointermove', move);
      document.removeEventListener('pointerup', stop);
      document.removeEventListener('pointercancel', stop);
      clearInterval(scroller);
      if (ev.type === 'pointercancel') { rows = base; render(); }
      ghost.remove();
      g.el.classList.remove('dragging');
      document.body.classList.remove('panels-dragging');
      remember();
    };
    document.addEventListener('pointermove', move);
    document.addEventListener('pointerup', stop);
    document.addEventListener('pointercancel', stop);
    update();
  }

  // ---- the period -------------------------------------------------------------
  const reloadAll = loadAll;
  function select(from, to) {
    selection = {from, to};
    rangeSelect.disabled = true;
    resetButton.hidden = false;
    note.textContent = 'Selected: ' + new Date(from).toLocaleString() + ' to ' + new Date(to).toLocaleString();
    reloadAll();
  }
  resetButton.addEventListener('click', () => {
    selection = null;
    rangeSelect.disabled = false;
    resetButton.hidden = true;
    note.textContent = '';
    reloadAll();
  });
  rangeSelect.addEventListener('change', () => { remember(); reloadAll(); });

  // ---- start up ---------------------------------------------------------------
  const layout = saved();
  if (layout.range && [...rangeSelect.options].some(o => o.value === layout.range)) {
    rangeSelect.value = layout.range;
  }
  rows = fromSaved(layout);
  if (!rows.length) rows = [[{g: addPanel(null), w: COLUMNS}]];
  render(true);

  // The charts' colours are read once, when drawn: redraw if the system
  // switches between light and dark.
  window.matchMedia('(prefers-color-scheme: dark)').addEventListener('change', () => {
    all().forEach(g => { g.plotKey = null; });
    reloadAll();
  });

  // Refresh every few seconds (not while the tab is hidden, and not while a
  // period is selected: that is fixed), and straight away on coming back.
  function tick() {
    if (!document.hidden && !selection) reloadAll();
  }
  document.addEventListener('visibilitychange', tick);
  const timer = setInterval(tick, REFRESH_MS);
  window.addEventListener('pagehide', () => clearInterval(timer));

  reloadAll();
})();
