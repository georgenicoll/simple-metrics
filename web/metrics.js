// The metrics chart: draws the selected metric over the selected time range
// with uPlot (served next to this file), and refreshes it every 5 seconds.
//
// Served by smweb, both on its own page and as a fragment that another web
// app embeds. The chart's element says where to fetch from (data-url), so the
// same script works wherever smweb is mounted.
//
// The server does all the summarising: the data URL returns at most 600
// points however long the range, each the average of its time bucket, with
// the smallest and largest value in the bucket alongside. The line is the
// average and the shaded band is min-to-max, so a short spike is still
// visible on a week-long chart. A gap (the collector was down, or a value
// couldn't be measured) is a null and is drawn as a break in the line.
(function () {
  'use strict';

  const REFRESH_MS = 5000;
  const metricSelect = document.getElementById('metric-select');
  const rangeSelect = document.getElementById('range-select');
  const chartBox = document.getElementById('chart');
  const status = document.getElementById('chart-status');
  // The page has no controls if the collector was unreachable when it loaded.
  if (!metricSelect || !rangeSelect || !chartBox) return;

  let plot = null;
  let plotKey = null;     // "<metric>|<range>": what the current chart was built for
  let inFlight = null;    // an AbortController, so a stale reply can't overwrite a newer choice
  let timer = null;

  // ---- remembering the last choice (nice to have; never required) ----------
  const STORE = 'simple-metrics';
  try {
    const saved = JSON.parse(localStorage.getItem(STORE) || '{}');
    if (saved.metric && [...metricSelect.options].some(o => o.value === saved.metric)) {
      metricSelect.value = saved.metric;
    }
    if (saved.range && [...rangeSelect.options].some(o => o.value === saved.range)) {
      rangeSelect.value = saved.range;
    }
  } catch (e) { /* storage may be unavailable: carry on without it */ }
  function remember() {
    try {
      localStorage.setItem(STORE, JSON.stringify({metric: metricSelect.value, range: rangeSelect.value}));
    } catch (e) { /* as above */ }
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

  // ---- the chart --------------------------------------------------------------
  function build(data) {
    if (plot) { plot.destroy(); plot = null; }
    const unit = data.metric.unit;
    const fmt = formatter(unit);
    const line = css('--pico-primary', '#0172ad');
    const text = css('--pico-muted-color', '#73818c');
    const grid = css('--pico-muted-border-color', '#e0e3e7');
    const shown = v => (v == null ? '-' : fmt(v));
    const options = {
      width: Math.max(chartBox.clientWidth, 280),
      height: 320,
      // The legend shows the values under the cursor, like a tooltip.
      cursor: {drag: {x: false, y: false}},
      scales: {x: {time: true}, y: {range: yRange(unit)}},
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
    plot = new uPlot(options, columns(data), chartBox);
  }

  // uPlot takes seconds, not milliseconds.
  function columns(data) {
    return [data.timestamps.map(t => t / 1000), data.avg, data.max, data.min];
  }

  function describe(data) {
    const points = data.timestamps.length;
    if (points === 0) return 'No data recorded for this range yet.';
    const last = data.timestamps[points - 1] / 1000;
    return 'Updated ' + clock(Date.now() / 1000) + ' - ' + points + ' points, ' +
      duration(data.step_ms) + ' each, newest at ' + clock(last);
  }

  function fail(message) {
    status.textContent = message;
    status.classList.add('error');
  }

  async function load() {
    if (inFlight) inFlight.abort();
    const controller = new AbortController();
    inFlight = controller;
    const key = metricSelect.value + '|' + rangeSelect.value;
    const url = chartBox.dataset.url + '?metric=' + encodeURIComponent(metricSelect.value) +
      '&range=' + encodeURIComponent(rangeSelect.value);
    try {
      const response = await fetch(url, {signal: controller.signal, credentials: 'same-origin'});
      if (response.redirected || response.status === 303 || response.status === 401) {
        // A web app in front of this has sent us elsewhere (its login has
        // expired, say): reload, so it can send the whole page there.
        window.location.reload();
        return;
      }
      const data = await response.json();
      if (!response.ok) {
        fail(data.error || 'Could not load the metrics.');
        return;
      }
      status.classList.remove('error');
      if (plot && plotKey === key) {
        plot.setData(columns(data));   // same chart: just move the data along
      } else {
        build(data);
        plotKey = key;
      }
      chartBox.setAttribute('aria-label', data.metric.label + ' over the last ' + rangeSelect.value);
      status.textContent = describe(data);
    } catch (e) {
      if (e.name !== 'AbortError') fail('Could not load the metrics.');
    } finally {
      if (inFlight === controller) inFlight = null;
    }
  }

  function changed() {
    remember();
    load();
  }
  metricSelect.addEventListener('change', changed);
  rangeSelect.addEventListener('change', changed);

  // Keep the chart the width of the page.
  window.addEventListener('resize', () => {
    if (plot) plot.setSize({width: Math.max(chartBox.clientWidth, 280), height: 320});
  });
  // The chart's colours are read once, when it is drawn: redraw if the
  // system switches between light and dark.
  window.matchMedia('(prefers-color-scheme: dark)').addEventListener('change', () => {
    plotKey = null;
    load();
  });

  // Refresh every few seconds, but not while the tab is hidden (and straight
  // away when it comes back).
  function tick() {
    if (!document.hidden) load();
  }
  document.addEventListener('visibilitychange', tick);
  timer = setInterval(tick, REFRESH_MS);
  window.addEventListener('pagehide', () => clearInterval(timer));

  load();
})();
