// The metrics charts: one or more graphs, each drawing its own metric over a
// shared time range with uPlot (served next to this file), refreshed every
// 5 seconds.
//
// Served by smweb, both on its own page and as a fragment that another web
// app embeds. The page says where to fetch from (data-url), so the same
// script works wherever smweb is mounted.
//
// The layout (which metric each graph shows, and the "Last" range) is kept in
// a cookie, so the next visit shows the same graphs. Dragging across a graph
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

  const graphs = [];      // {el, select, box, status, del, plot, plotKey}
  let selection = null;   // {from, to} in ms while a period is selected on a chart

  // ---- remembering the layout (a cookie; nice to have, never required) -------
  function remember() {
    try {
      const layout = {range: rangeSelect.value, metrics: graphs.map(g => g.select.value)};
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
      width: Math.max(g.box.clientWidth, 280),
      height: 260,
      // Dragging across the chart selects a period (see the setSelect hook).
      // All graphs share the cursor: pointing at a time on one shows that
      // time (and its values) on every graph.
      cursor: {y: false, drag: {x: true, y: false, setScale: false}, sync: {key: 'sm', scales: ['x', null]}},
      select: {show: true},
      hooks: {
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
        {label: 'max', stroke: 'transparent', width: 0, value: (u, v) => shown(v), points: {show: false}},
        {label: 'min', stroke: 'transparent', width: 0, value: (u, v) => shown(v), points: {show: false}},
      ],
      // The shaded band between the smallest and the largest value per bucket.
      bands: [{series: [2, 3], fill: line + '33'}],
    };
    g.plot = new uPlot(options, columns(data), g.box);
  }

  // uPlot takes seconds, not milliseconds.
  function columns(data) {
    return [data.timestamps.map(t => t / 1000), data.avg, data.max, data.min];
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
    graphs.forEach(g => {
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
    const names = [...new Set(graphs.map(g => g.select.value))];
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
      graphs.forEach(g => {
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

  // ---- the set of graphs -------------------------------------------------------
  function addGraph(metric) {
    const el = template.content.firstElementChild.cloneNode(true);
    const g = {
      el,
      select: el.querySelector('.metric-select'),
      box: el.querySelector('.chart'),
      status: el.querySelector('.chart-status'),
      del: el.querySelector('.graph-delete'),
      plot: null, plotKey: null,
    };
    if (metric && [...g.select.options].some(o => o.value === metric)) g.select.value = metric;
    g.select.addEventListener('change', () => { remember(); loadAll(); });
    g.del.addEventListener('click', () => {
      if (graphs.length < 2) return;
      if (g.plot) g.plot.destroy();
      graphs.splice(graphs.indexOf(g), 1);
      el.remove();
      updateDeletable();
      remember();
    });
    graphs.push(g);
    graphsBox.appendChild(el);
    updateDeletable();
    return g;
  }
  // The last remaining graph can't be removed.
  function updateDeletable() {
    graphs.forEach(g => { g.del.hidden = graphs.length < 2; });
  }
  addButton.addEventListener('click', () => {
    // A new graph starts as a copy of the last one's metric; change it from there.
    const g = addGraph(graphs[graphs.length - 1].select.value);
    remember();
    loadAll();
    g.el.scrollIntoView({block: 'nearest', behavior: 'smooth'});
  });

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
  const wanted = Array.isArray(layout.metrics) && layout.metrics.length ? layout.metrics.slice(0, 20) : [null];
  wanted.forEach(addGraph);

  // Keep the charts the width of the page.
  window.addEventListener('resize', () => {
    graphs.forEach(g => { if (g.plot) g.plot.setSize({width: Math.max(g.box.clientWidth, 280), height: 260}); });
  });
  // The charts' colours are read once, when drawn: redraw if the system
  // switches between light and dark.
  window.matchMedia('(prefers-color-scheme: dark)').addEventListener('change', () => {
    graphs.forEach(g => { g.plotKey = null; });
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
