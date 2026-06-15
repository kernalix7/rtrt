const OVERVIEW_SOURCE_ORDER = ['output_optimizer', 'memory', 'command_optimizer'];
const OVERVIEW_SOURCE_LABELS = {
  memory: 'Memory',
  output_optimizer: 'Output Optimizer',
  command_optimizer: 'Command Optimizer',
};
// Status-line savings pillars: glyph + the metric each pillar reports.
const OVERVIEW_SOURCE_PILLARS = {
  output_optimizer: { glyph: '📝', metric: 'terse level + rule compress' },
  memory: { glyph: '🧠', metric: 'storage-reduction %' },
  command_optimizer: { glyph: '⚡', metric: 'effective %' },
};
const OVERVIEW_WINDOWS = new Set(['1h', '6h', '24h', '7d', '30d', 'all']);
let overviewPollTimer = null;
let overviewAgeTimer = null;
let overviewLastUpdatedAt = 0;
let overviewLoading = false;
let overviewWindow = OVERVIEW_WINDOWS.has(localStorage.getItem('rtrt.overview.window'))
  ? localStorage.getItem('rtrt.overview.window')
  : 'all';

function numberValue(v) {
  const n = Number(v || 0);
  return Number.isFinite(n) ? n : 0;
}

function pctValue(v) {
  const n = Number(v);
  return Number.isFinite(n) ? n : 0;
}

function fmtPct(v) {
  return `${pctValue(v).toFixed(1)}%`;
}

function fmtPctMaybe(v) {
  return v === null || v === undefined ? 'n/a' : fmtPct(v);
}

function overviewWindowLabel(value) {
  return value === 'all' ? 'All' : value;
}

function syncOverviewWindowButtons() {
  document.querySelectorAll('#overview-window-selector button').forEach(btn => {
    btn.classList.toggle('active', btn.dataset.window === overviewWindow);
  });
  const label = document.getElementById('kpi-window-label');
  if (label) label.textContent = `window: ${overviewWindowLabel(overviewWindow)}`;
}

function coverageValue(source) {
  const c = source && source.coverage ? source.coverage : {};
  return {
    reduced: numberValue(c.reduced),
    total: numberValue(c.total),
  };
}

function coverageLead(source) {
  const c = coverageValue(source);
  const passed = Math.max(0, c.total - c.reduced);
  if (!c.total) return 'no measured items';
  return `${c.reduced.toLocaleString()} of ${c.total.toLocaleString()} — ${passed.toLocaleString()} passed through`;
}

function withoutWithSaved(source) {
  const original = numberValue(source.original_chars);
  const withRtrt = numberValue(source.with_rtrt_chars);
  const saved = numberValue(source.saved_chars);
  const tokens = numberValue(source.saved_tokens);
  return `${original.toLocaleString()} without rtrt → ${withRtrt.toLocaleString()} with rtrt → ${saved.toLocaleString()} saved (${tokens.toLocaleString()} tokens, ${fmtPctMaybe(source.saved_pct)})`;
}

function percentInline(pct, maxPct) {
  const width = maxPct > 0 ? Math.max(0, Math.min(100, pctValue(pct) / maxPct * 100)) : 0;
  return `<div class="percent-inline"><span>${fmtPct(pct)}</span><span class="percent-bar"><span style="width:${width.toFixed(1)}%;"></span></span></div>`;
}

function setUpdating(el) {
  if (!el) return;
  el.classList.add('updating');
  setTimeout(() => el.classList.remove('updating'), 220);
}

function updateOverviewLiveIndicator() {
  const el = document.getElementById('overview-live');
  if (!el) return;
  if (!overviewLastUpdatedAt) {
    el.textContent = 'live';
    return;
  }
  const age = Math.max(0, Math.floor((Date.now() - overviewLastUpdatedAt) / 1000));
  el.textContent = `live · updated ${age}s ago`;
}

function orderedSources(payload) {
  const sources = payload && Array.isArray(payload.sources) ? payload.sources : [];
  const byName = new Map(sources.map(s => [s.name, s]));
  return OVERVIEW_SOURCE_ORDER.map(name => byName.get(name) || {
    name,
    label: OVERVIEW_SOURCE_LABELS[name],
    available: false,
    count: 0,
    saved_chars: 0,
    saved_tokens: 0,
    original_chars: 0,
    with_rtrt_chars: 0,
    saved_pct: null,
    effective_pct: null,
    coverage: { reduced: 0, total: 0 },
    by_project: [],
  });
}

function normalizedOptimizerLevel(level) {
  const value = String(level || 'off').trim().toLowerCase();
  return ['lite', 'full', 'ultra'].includes(value) ? value : 'off';
}

function isOptimizerActive(level, active) {
  if (typeof active === 'boolean') return active;
  return normalizedOptimizerLevel(level) !== 'off';
}

function lowerFirst(value) {
  return value ? value.charAt(0).toLowerCase() + value.slice(1) : value;
}

function optimizerMeasurementNote(source = {}) {
  return lowerFirst(source.measurement_note || OUTPUT_OPTIMIZER_MEASUREMENT_NOTE);
}

function optimizerRuleSummary(source = {}) {
  const pct = source.saved_pct == null ? 'n/a' : fmtPct(source.saved_pct);
  const count = numberValue(source.count);
  return `rule compressor: ${pct} overall on ${count.toLocaleString()} measured calls (${optimizerMeasurementNote(source)})`;
}

function optimizerLevelBadge(level, active) {
  const normalized = normalizedOptimizerLevel(level);
  const enabled = isOptimizerActive(normalized, active);
  const label = enabled ? `Terse mode: ${normalized.toUpperCase()} (active)` : 'OFF';
  const cls = enabled ? 'badge ok' : 'badge optimizer-level-status inactive';
  return `<span class="${cls}">${escapeHtml(label)}</span>`;
}

function renderOverviewSources(payload) {
  const wrap = document.getElementById('optimizer-source-cards');
  if (!wrap) return;
  const sources = orderedSources(payload);
  const totalTokens = Math.max(0, numberValue(payload && payload.total_saved_tokens));
  wrap.innerHTML = sources.map(source => {
    const tokens = numberValue(source.saved_tokens);
    const chars = numberValue(source.saved_chars);
    const effective = source.effective_pct;
    const coverage = coverageValue(source);
    const share = totalTokens > 0 ? Math.max(0, tokens) / totalTokens * 100 : 0;
    const badge = source.available === false
      ? '<span class="badge warn">unavailable</span>'
      : '<span class="badge ok">available</span>';
    const pillar = OVERVIEW_SOURCE_PILLARS[source.name] || { glyph: '•', metric: 'savings' };
    const name = `<span class="name"><span class="pillar-glyph" aria-hidden="true">${pillar.glyph}</span> ${escapeHtml(source.label || source.name)}</span>`;
    const pillarMetric = `<div class="overall pillar-metric">${escapeHtml(pillar.metric)}</div>`;
    if (source.name === 'output_optimizer') {
      return `<div class="optimizer-mini-card">
        <div class="top">${name}${badge}</div>
        ${pillarMetric}
        <div class="optimizer-level-primary metric-value">${optimizerLevelBadge(source.level, source.active)}</div>
        <div class="metric-lead metric-value">${fmtPctMaybe(effective)} effective</div>
        <div class="meta"><span>${coverageLead(source)}</span></div>
        <div class="flow"><span>${withoutWithSaved(source)}</span><span class="overall">${tokens.toLocaleString()} tokens · ${chars.toLocaleString()} chars saved · ${optimizerRuleSummary(source)}</span></div>
        <div class="share-bar" title="${share.toFixed(1)}% of total"><span style="width:${Math.max(0, Math.min(100, share)).toFixed(1)}%;"></span></div>
      </div>`;
    }
    return `<div class="optimizer-mini-card">
      <div class="top">${name}${badge}</div>
      ${pillarMetric}
      <div class="metric-lead metric-value">${fmtPctMaybe(effective)} effective</div>
      <div class="meta"><span>${coverageLead(source)}</span></div>
      <div class="flow"><span>${withoutWithSaved(source)}</span><span class="overall">overall ${fmtPctMaybe(source.saved_pct)} blended · ${tokens.toLocaleString()} tokens · ${chars.toLocaleString()} chars saved</span></div>
      <div class="share-bar" title="${share.toFixed(1)}% of total"><span style="width:${Math.max(0, Math.min(100, share)).toFixed(1)}%;"></span></div>
    </div>`;
  }).join('');
  wrap.querySelectorAll('.metric-value').forEach(setUpdating);
}

function projectDisplayCutoff(total, sourceCount) {
  if (total <= 0) return 0;
  const root = Math.ceil(Math.sqrt(total));
  const sourceFloor = Math.max(1, sourceCount);
  const lower = sourceFloor * 2;
  const upper = Math.max(lower, sourceFloor * sourceFloor * sourceFloor);
  if (total <= upper) return total;
  return Math.min(total, Math.max(lower, Math.min(upper, root)));
}

function sourceRollup(project, sourceName) {
  return project && project.by_source && project.by_source[sourceName]
    ? project.by_source[sourceName]
    : { count: 0, saved_chars: 0, saved_tokens: 0 };
}

function optimizerCell(project, sourceName) {
  const item = sourceRollup(project, sourceName);
  const tokens = numberValue(item.saved_tokens);
  const chars = numberValue(item.saved_chars);
  const original = numberValue(item.original_chars);
  const withRtrt = numberValue(item.with_rtrt_chars);
  const cov = item.coverage || { reduced: 0, total: item.count || 0 };
  return `<td class="tokens-cell">${tokens.toLocaleString()}<div class="hint">${original.toLocaleString()} → ${withRtrt.toLocaleString()} · ${chars.toLocaleString()} saved</div><div class="hint">${numberValue(cov.reduced).toLocaleString()} / ${numberValue(cov.total).toLocaleString()} reduced</div></td>`;
}

function renderProjectSavings(payload) {
  const body = document.querySelector('#project-savings-tbl tbody');
  const hint = document.getElementById('project-savings-hint');
  if (!body) return;
  const projects = payload && Array.isArray(payload.projects) ? payload.projects : [];
  if (!projects.length) {
    body.innerHTML = '<tr><td colspan="6" class="empty">No persisted savings yet.</td></tr>';
    if (hint) hint.textContent = 'Sorted by total tokens.';
    return;
  }
  const cutoff = projectDisplayCutoff(projects.length, OVERVIEW_SOURCE_ORDER.length);
  const visible = projects.slice(0, cutoff);
  const maxTokens = Math.max(1, ...visible.map(p => Math.max(0, numberValue(p.saved_tokens))));
  const maxPct = Math.max(0, ...visible.map(p => pctValue(p.saved_pct)));
  body.innerHTML = visible.map(project => {
    const totalTokens = numberValue(project.saved_tokens);
    const width = Math.max(0, totalTokens) / maxTokens * 100;
    const pct = pctValue(project.saved_pct);
    return `<tr>
      <td class="project-name-cell">${escapeHtml(project.project || '(unknown)')}</td>
      ${optimizerCell(project, 'output_optimizer')}
      ${optimizerCell(project, 'memory')}
      ${optimizerCell(project, 'command_optimizer')}
      <td class="percent-cell">${percentInline(pct, maxPct)}</td>
      <td class="project-total-cell"><span class="tokens-cell">${totalTokens.toLocaleString()}</span><div class="project-row-bar"><span style="width:${Math.max(0, Math.min(100, width)).toFixed(1)}%;"></span></div></td>
    </tr>`;
  }).join('');
  if (hint) {
    hint.textContent = cutoff < projects.length
      ? `Showing ${cutoff.toLocaleString()} of ${projects.length.toLocaleString()}, sorted by total tokens.`
      : `Showing all ${projects.length.toLocaleString()} projects, sorted by total tokens.`;
  }
}

async function loadOverview() {
  if (overviewLoading) return;
  overviewLoading = true;
  const overviewParams = new URLSearchParams();
  if (currentProject() && !isGlobalScope()) overviewParams.set('project', currentProject());
  overviewParams.set('window', overviewWindow);
  const overviewUrl = `/api/overview${overviewParams.toString() ? '?' + overviewParams.toString() : ''}`;
  let mRes = {summary:{}, recent:[]};
  let bRes = null;
  let oRes = null;
  try {
    [mRes, bRes, oRes] = await Promise.all([
      fetch('/api/metrics').then(r => r.ok ? r.json() : ({summary:{}, recent:[]})).catch(() => ({summary:{}, recent:[]})),
      fetch('/api/budget').then(r => r.ok ? r.json() : null).catch(() => null),
      fetch(overviewUrl).then(r => r.ok ? r.json() : null).catch(() => null),
    ]);
  } finally {
    overviewLoading = false;
  }
  const s = mRes.summary || {};
  const calls = s.calls || 0;
  const savedChars = oRes ? Number(oRes.total_saved_chars || 0) : 0;
  const savedTokens = oRes && oRes.total_saved_tokens != null ? Number(oRes.total_saved_tokens) : 0;
  const savedPct = oRes && oRes.total_saved_pct != null ? Number(oRes.total_saved_pct) : null;
  const avgLatency = calls ? (s.total_latency_ms / Math.max(1, calls)).toFixed(0) : '—';
  syncOverviewWindowButtons();
  document.getElementById('kpi-saved').textContent = `${fmtPctMaybe(savedPct)} saved`;
  animateCount(document.getElementById('kpi-saved-tokens'), savedTokens);
  animateCount(document.getElementById('kpi-saved-chars'), savedChars);
  animateCount(document.getElementById('kpi-calls'), calls);
  setUpdating(document.getElementById('kpi-saved'));
  document.getElementById('kpi-saved-sub').textContent = '(estimate, chars/4)';
  document.getElementById('kpi-latency').textContent = `${avgLatency} ms`;
  document.getElementById('kpi-spent').textContent = bRes ? fmtUsd(bRes.spent_usd) : '—';
  document.getElementById('kpi-spent-sub').textContent = bRes && bRes.cap_usd ? `cap ${fmtUsd(bRes.cap_usd)}` : 'no cap set';

  renderOverviewSources(oRes);
  renderProjectSavings(oRes);
  const notes = document.getElementById('optimizer-notes');
  if (notes) {
    notes.textContent = oRes && oRes.note ? oRes.note : 'All available counters loaded.';
  }
  overviewLastUpdatedAt = Date.now();
  updateOverviewLiveIndicator();

  setPill('pill-gateway', calls > 0, calls > 0 ? 'gateway active' : 'gateway idle');
  setPill('pill-memory', null, 'Memory');
  if (bRes) {
    const cache = (bRes.cache_len === null || bRes.cache_len === undefined) ? 'off' : `${bRes.cache_len} entries`;
    setPill('pill-cache', bRes.cache_len !== null && bRes.cache_len !== undefined, `Cache ${cache}`);
    document.getElementById('env-cache').textContent = cache;
    document.getElementById('env-budget').textContent = bRes.cap_usd ? `${fmtUsd(bRes.cap_usd)} (${fmtUsd(bRes.spent_usd)} spent)` : 'not set';
  }

  const recent = mRes.recent || [];
  spark('chart-latency', recent.slice().reverse().map(r => r.latency_ms), '#2962FF');
  spark('chart-tokens', recent.slice().reverse().map(r => (r.usage.input_tokens || 0) + (r.usage.output_tokens || 0)), '#16a34a');

  const byParent = new Map(); const heads = [];
  for (const r of recent) {
    if (r.parent_id) {
      let arr = byParent.get(r.parent_id);
      if (!arr) { arr = []; byParent.set(r.parent_id, arr); }
      arr.push(r);
    } else { heads.push(r); }
  }
  function row(r, depth) {
    const t = new Date(r.started_at * 1000).toTimeString().slice(0, 8);
    const status = r.ok ? '<span class="badge ok">ok</span>' : `<span class="badge err">${r.error || 'failed'}</span>`;
    const ind = depth ? `<span style="color:var(--muted);">└─ </span>` : '';
    return `<tr><td>${ind}${t}</td><td>${r.provider}</td><td><code>${r.model}</code></td><td>${r.usage.input_tokens}</td><td>${r.usage.output_tokens}</td><td>${r.latency_ms} ms</td><td>${status}</td></tr>`;
  }
  const rows = [];
  for (const h of heads) {
    rows.push(row(h, 0));
    for (const c of (byParent.get(h.id) || [])) rows.push(row(c, 1));
  }
  document.querySelector('#recent-tbl tbody').innerHTML = rows.join('') || '<tr><td colspan="7" class="empty">No calls yet. Start with <code>rtrt provider chat</code> or MCP.</td></tr>';
}
function startOverviewPolling() {
  if (activePage() !== 'overview') return;
  if (!overviewPollTimer) {
    loadOverview();
    overviewPollTimer = setInterval(() => {
      if (activePage() === 'overview') loadOverview();
      else stopOverviewPolling();
    }, 5000);
  }
  if (!overviewAgeTimer) {
    overviewAgeTimer = setInterval(updateOverviewLiveIndicator, 1000);
  }
}

function stopOverviewPolling() {
  if (overviewPollTimer) {
    clearInterval(overviewPollTimer);
    overviewPollTimer = null;
  }
  if (overviewAgeTimer) {
    clearInterval(overviewAgeTimer);
    overviewAgeTimer = null;
  }
}

const GAIN_REFRESH_MS = 5000;
const COMMAND_COVERED_FILTERS = [
  'git status', 'git diff', 'git show', 'git branch', 'git stash', 'git log',
  'cargo nextest', 'cargo check', 'cargo clippy', 'cargo build', 'cargo test',
  'ls -la', 'ls -al', 'ls', 'grep -rn', 'grep', 'rg', 'find', 'cat', 'read',
  'curl', 'wget', 'gh', 'docker', 'kubectl', 'pytest', 'go test',
  'npm', 'npx', 'pnpm', 'pip', 'tsc', 'eslint', 'prettier',
];
let gainPollTimer = null;
let gainAgeTimer = null;
let gainLastUpdatedAt = 0;
let gainLoading = false;

function commandGainVisible() {
  return activePage() === 'command' && !document.getElementById('sub-command-gain').hidden;
}

function updateGainLiveIndicator() {
  const el = document.getElementById('command-gain-live');
  if (!el) return;
  if (!gainLastUpdatedAt) {
    el.textContent = 'live';
    return;
  }
  const age = Math.max(0, Math.floor((Date.now() - gainLastUpdatedAt) / 1000));
  el.textContent = `live · updated ${age}s ago`;
}

function renderCommandCoverage() {
  const list = document.getElementById('command-coverage-list');
  if (!list || list.dataset.rendered === '1') return;
  list.innerHTML = COMMAND_COVERED_FILTERS.map(cmd => `<span class="badge">${escapeHtml(cmd)}</span>`).join('');
  list.dataset.rendered = '1';
}

function gainUrl() {
  const params = new URLSearchParams();
  if (currentProject() && !isGlobalScope()) params.set('project', currentProject());
  params.set('window', overviewWindow);
  return `/api/gain${params.toString() ? '?' + params.toString() : ''}`;
}

function renderGainUnavailable(d) {
  document.getElementById('gain-unavailable').hidden = false;
  document.getElementById('gain-body').hidden = true;
  document.getElementById('gain-unavailable-text').textContent =
    d && d.reason ? `${d.reason}${d.path ? ` · ${d.path}` : ''}` : 'No Command Optimizer database yet.';
  animateCount(document.getElementById('gain-total-tokens'), 0);
  animateCount(document.getElementById('gain-total-chars'), 0);
  animateCount(document.getElementById('gain-total-runs'), 0);
  document.getElementById('gain-saved-pct').textContent = '0.0%';
  document.getElementById('gain-display-count').textContent = 'waiting for data';
}

function renderGainTopCommands(rows) {
  const tbody = document.querySelector('#gain-top-tbl tbody');
  const maxSaved = Math.max(1, ...rows.map(r => numberValue(r.saved_chars)));
  const maxPct = Math.max(0, ...rows.map(r => pctValue(r.saved_pct)));
  tbody.innerHTML = rows.map(r => {
    const chars = numberValue(r.saved_chars);
    const width = Math.max(0, Math.min(100, (chars / maxSaved) * 100));
    const pct = pctValue(r.saved_pct);
    return `<tr>
      <td class="project-name-cell"><code>${escapeHtml(r.command || '')}</code></td>
      <td class="tokens-cell">${numberValue(r.runs).toLocaleString()}</td>
      <td class="command-gain-bar">
        <span class="tokens-cell">${numberValue(r.saved_tokens).toLocaleString()} tokens</span>
        <span class="hint">${numberValue(r.input_chars).toLocaleString()} without rtrt → ${numberValue(r.output_chars).toLocaleString()} with rtrt → ${chars.toLocaleString()} saved</span>
        <span class="hint">${coverageLead(r)} · effective ${fmtPctMaybe(r.effective_pct)}</span>
        <span class="bar"><span style="width:${width.toFixed(1)}%;"></span></span>
      </td>
      <td class="percent-cell">${percentInline(pct, maxPct)}</td>
    </tr>`;
  }).join('') || '<tr><td colspan="4" class="empty">No command savings yet.</td></tr>';
}

function renderGainProjects(rows) {
  const tbody = document.querySelector('#gain-project-tbl tbody');
  const maxPct = Math.max(0, ...rows.map(r => pctValue(r.saved_pct)));
  tbody.innerHTML = rows.map(r => `<tr>
    <td class="project-name-cell">${escapeHtml(r.project || '(unknown)')}</td>
    <td class="tokens-cell">${numberValue(r.runs).toLocaleString()}</td>
    <td class="percent-cell">${percentInline(r.saved_pct, maxPct)}</td>
    <td class="tokens-cell">${numberValue(r.saved_chars).toLocaleString()}<div class="hint">${numberValue(r.input_chars).toLocaleString()} without rtrt → ${numberValue(r.output_chars).toLocaleString()} with rtrt</div><div class="hint">${coverageLead(r)} · effective ${fmtPctMaybe(r.effective_pct)}</div></td>
    <td class="tokens-cell">${numberValue(r.saved_tokens).toLocaleString()}</td>
  </tr>`).join('') || '<tr><td colspan="5" class="empty">No project breakdown yet.</td></tr>';
}

function renderGainHistory(rows) {
  const tbody = document.querySelector('#gain-history-tbl tbody');
  tbody.innerHTML = rows.map(r => `<tr>
    <td class="tokens-cell">${escapeHtml(r.ts || '—')}</td>
    <td class="project-name-cell">${escapeHtml(r.project || '(unknown)')}</td>
    <td class="project-name-cell"><code>${escapeHtml(r.original_cmd || '')}</code></td>
    <td><span class="badge">${escapeHtml(r.mode || 'command')}</span></td>
    <td class="tokens-cell">${numberValue(r.saved_tokens).toLocaleString()} tokens<div class="hint">${numberValue(r.input_chars).toLocaleString()} without rtrt → ${numberValue(r.output_chars).toLocaleString()} with rtrt → ${numberValue(r.saved_chars).toLocaleString()} saved · ${fmtPct(r.saved_pct)}</div></td>
    <td class="tokens-cell">${numberValue(r.exec_ms).toLocaleString()} ms</td>
  </tr>`).join('') || '<tr><td colspan="6" class="empty">No recent proxy runs yet.</td></tr>';
}

async function loadGain() {
  if (gainLoading) return;
  gainLoading = true;
  let d = null;
  try {
    const r = await fetch(gainUrl());
    d = r.ok ? await r.json() : { available: false, reason: `HTTP ${r.status}` };
  } catch (e) {
    d = { available: false, reason: e.message || String(e) };
  } finally {
    gainLoading = false;
  }

  gainLastUpdatedAt = Date.now();
  updateGainLiveIndicator();
  if (!d || d.available === false) {
    renderGainUnavailable(d);
    return;
  }
  document.getElementById('gain-unavailable').hidden = true;
  document.getElementById('gain-body').hidden = false;
  animateCount(document.getElementById('gain-total-tokens'), numberValue(d.total_saved_tokens));
  animateCount(document.getElementById('gain-total-chars'), numberValue(d.total_saved_chars));
  animateCount(document.getElementById('gain-total-runs'), numberValue(d.total_runs));
  setUpdating(document.getElementById('gain-total-tokens'));
  document.getElementById('gain-token-note').textContent = `(${d.token_estimate || 'chars/4'} estimate)`;
  document.getElementById('gain-saved-pct').textContent = `${fmtPctMaybe(d.effective_pct)} effective · ${coverageLead(d)} · overall ${fmtPct(d.saved_pct)}`;
  document.getElementById('gain-display-count').textContent = `${numberValue(d.display_count).toLocaleString()} rows shown`;
  renderGainTopCommands(Array.isArray(d.top_commands) ? d.top_commands : []);
  renderGainProjects(Array.isArray(d.per_project) ? d.per_project : []);
  renderGainHistory(Array.isArray(d.recent_history) ? d.recent_history : []);
}

function startGainPolling() {
  if (!commandGainVisible()) return;
  if (!gainPollTimer) {
    loadGain();
    gainPollTimer = setInterval(() => {
      if (commandGainVisible()) loadGain();
      else stopGainPolling();
    }, GAIN_REFRESH_MS);
  }
  if (!gainAgeTimer) {
    gainAgeTimer = setInterval(updateGainLiveIndicator, 1000);
  }
}

function stopGainPolling() {
  if (gainPollTimer) {
    clearInterval(gainPollTimer);
    gainPollTimer = null;
  }
  if (gainAgeTimer) {
    clearInterval(gainAgeTimer);
    gainAgeTimer = null;
  }
}

// Group detected tools by their `kind` field from /api/detect.
// Readable section titles; any kind the endpoint may add later falls into "Other".
const ENV_KIND_ORDER = ['CodingAgent', 'LocalRuntime', 'ProviderApi', 'McpServer'];
const ENV_KIND_LABELS = {
  CodingAgent: 'Coding Agents',
  LocalRuntime: 'Local Runtimes',
  ProviderApi: 'Provider APIs',
  McpServer: 'MCP Servers',
};
const ENV_OTHER_KIND = 'Other';
let environmentLoading = false;

function envList(values) {
  return Array.isArray(values) && values.length
    ? values.map(v => `<span class="badge">${escapeHtml(v)}</span>`).join(' ')
    : '<span class="hint">—</span>';
}

function envKindLabel(kind) {
  if (ENV_KIND_LABELS[kind]) return ENV_KIND_LABELS[kind];
  return kind === ENV_OTHER_KIND ? ENV_OTHER_KIND : String(kind || ENV_OTHER_KIND);
}

// Enabled-state badge: active vs disabled, per the task spec (separate from installed).
function envEnabledBadge(enabled) {
  return enabled
    ? '<span class="badge ok">active</span>'
    : '<span class="badge warn">disabled</span>';
}

function envToolRow(tool) {
  const enabled = !!tool.enabled;
  const toggleClass = enabled ? 'enabled' : 'disabled';
  const installed = tool.installed ? '<span class="badge ok">yes</span>' : '<span class="badge warn">no</span>';
  const models = Array.isArray(tool.models) && tool.models.length
    ? tool.models.map(m => `<code>${escapeHtml(m)}</code>`).join(' ')
    : '<span class="hint">—</span>';
  const modes = envList(tool.invocation_modes);
  const template = tool.cli_invocation || tool.config_path || tool.path || '';
  return `<tr>
    <td class="project-name-cell">${escapeHtml(tool.name || 'unknown')}</td>
    <td>${envEnabledBadge(enabled)}</td>
    <td>${installed}</td>
    <td>${tool.version ? `<code>${escapeHtml(tool.version)}</code>` : '<span class="hint">—</span>'}</td>
    <td>${modes}</td>
    <td><span class="badge">${escapeHtml(tool.cost_class || 'Unknown')}</span></td>
    <td class="env-models">${models}</td>
    <td class="env-template">${template ? `<code>${escapeHtml(template)}</code>` : '<span class="hint">—</span>'}</td>
    <td><button type="button" class="env-toggle ${toggleClass}" data-tool="${escapeAttr(tool.name || '')}" data-enabled="${enabled ? '1' : '0'}">${enabled ? 'Enabled' : 'Disabled'}</button></td>
  </tr>`;
}

function envGroupCard(kind, groupTools) {
  const active = groupTools.filter(t => t.enabled).length;
  const sorted = groupTools.slice().sort((a, b) => String(a.name || '').localeCompare(String(b.name || '')));
  const body = sorted.length
    ? sorted.map(envToolRow).join('')
    : '<tr><td colspan="9" class="empty">none</td></tr>';
  return `<div class="card env-group-card">
    <div class="head">
      <h2>${escapeHtml(envKindLabel(kind))}</h2>
      <span class="hint">${active.toLocaleString()} active · ${groupTools.length.toLocaleString()} detected</span>
    </div>
    <div class="env-table-wrap">
      <table class="environment-tbl">
        <thead><tr><th>Name</th><th>State</th><th>Installed</th><th>Version</th><th>Modes</th><th>Cost class</th><th>Models</th><th>Invocation template</th><th>Enabled</th></tr></thead>
        <tbody>${body}</tbody>
      </table>
    </div>
  </div>`;
}

function renderEnvironment(tools) {
  const wrap = document.getElementById('environment-groups');
  const notes = document.getElementById('environment-notes');
  if (!wrap) return;
  if (!Array.isArray(tools) || !tools.length) {
    wrap.innerHTML = '<div class="empty">No tools detected.</div>';
    if (notes) notes.textContent = 'Detection completed with no rows.';
    return;
  }
  const groups = new Map();
  for (const tool of tools) {
    const raw = tool.kind || ENV_OTHER_KIND;
    const kind = ENV_KIND_LABELS[raw] ? raw : ENV_OTHER_KIND;
    if (!groups.has(kind)) groups.set(kind, []);
    groups.get(kind).push(tool);
  }
  // Known kinds in canonical order, then "Other" last (only if present).
  const orderedKinds = [
    ...ENV_KIND_ORDER.filter(kind => groups.has(kind)),
    ...(groups.has(ENV_OTHER_KIND) ? [ENV_OTHER_KIND] : []),
  ];
  wrap.innerHTML = orderedKinds.map(kind => envGroupCard(kind, groups.get(kind))).join('');
  wrap.querySelectorAll('.env-toggle').forEach(btn => {
    btn.onclick = () => toggleEnvironmentTool(btn.dataset.tool, btn.dataset.enabled !== '1');
  });
  if (notes) notes.textContent = `${tools.length.toLocaleString()} detected tools across ${orderedKinds.length.toLocaleString()} groups. API keys are never displayed.`;
}

async function loadEnvironment() {
  if (environmentLoading) return;
  environmentLoading = true;
  const wrap = document.getElementById('environment-groups');
  if (wrap) wrap.innerHTML = '<div class="empty">Loading…</div>';
  try {
    const r = await fetch('/api/detect');
    if (!r.ok) {
      const text = await r.text();
      throw new Error(`HTTP ${r.status}: ${text}`);
    }
    renderEnvironment(await r.json());
  } catch (e) {
    if (wrap) wrap.innerHTML = `<div class="empty">${escapeHtml(e.message || String(e))}</div>`;
  } finally {
    environmentLoading = false;
  }
}

async function toggleEnvironmentTool(name, enabled) {
  if (!name) return;
  try {
    const r = await fetch('/api/detect/toggle', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ name, enabled }),
    });
    if (!r.ok) {
      const text = await r.text();
      throw new Error(`HTTP ${r.status}: ${text}`);
    }
    await loadEnvironment();
    showToast(`${name} ${enabled ? 'enabled' : 'disabled'}`, 'ok');
  } catch (e) {
    showToast(`Environment update failed: ${e.message || e}`, 'err');
  }
}

document.getElementById('gain-refresh').onclick = () => { loadGain(); pushActivity('Command gain refreshed'); };
document.getElementById('overview-refresh').onclick = () => { loadOverview(); pushActivity('Overview refreshed'); };
document.getElementById('environment-refresh').onclick = () => { loadEnvironment(); pushActivity('Environment refreshed'); };
document.querySelectorAll('#overview-window-selector button').forEach(btn => {
  btn.onclick = () => {
    const next = OVERVIEW_WINDOWS.has(btn.dataset.window) ? btn.dataset.window : 'all';
    overviewWindow = next;
    localStorage.setItem('rtrt.overview.window', overviewWindow);
    syncOverviewWindowButtons();
    loadOverview();
    if (commandGainVisible()) loadGain();
    pushActivity(`overview window · ${overviewWindowLabel(overviewWindow)}`);
  };
});

function routeValue(value) {
  if (value === null || value === undefined || value === '') return '—';
  return String(value);
}

function routeCostBadge(costClass) {
  const label = routeValue(costClass);
  const cls = label === 'LocalFree' || label === 'SubscriptionFlat' ? 'ok' : (label === 'ApiMetered' ? 'warn' : '');
  return `<span class="badge ${cls}">${escapeHtml(label)}</span>`;
}

function routeCapabilities(caps) {
  return Array.isArray(caps) && caps.length
    ? caps.map(c => `<span class="badge">${escapeHtml(String(c))}</span>`).join(' ')
    : '';
}

function renderRouteResult(data) {
  const chosen = data.chosen || {};
  const alternatives = Array.isArray(data.alternatives) ? data.alternatives : [];
  const usage = data.usage_headroom || {};
  const byTarget = usage.by_target || {};
  // The chosen target's headroom (if the endpoint reports usage limits for it)
  // is part of "why" — surface it alongside cost class / mode / reason.
  const chosenHeadroom = (chosen.target && byTarget[chosen.target] && byTarget[chosen.target].label)
    ? byTarget[chosen.target].label
    : null;
  document.getElementById('route-results').hidden = false;
  document.getElementById('route-chosen').innerHTML = `
    <div class="route-chosen-card">
      <div class="route-chosen-top">
        <div class="route-target-name">${escapeHtml(routeValue(chosen.target))}</div>
        ${routeCostBadge(chosen.cost_class)}
      </div>
      <div class="route-meta-grid">
        <div class="route-meta-box"><div class="label">mode</div><div class="value">${escapeHtml(routeValue(chosen.mode))}</div></div>
        <div class="route-meta-box"><div class="label">model</div><div class="value">${escapeHtml(routeValue(chosen.model))}</div></div>
        <div class="route-meta-box"><div class="label">headroom</div><div class="value">${escapeHtml(chosenHeadroom || 'n/a')}</div></div>
        <div class="route-meta-box"><div class="label">why</div><div class="value">${escapeHtml(routeValue(chosen.reason))}</div></div>
      </div>
    </div>`;
  document.getElementById('route-alternatives').innerHTML = alternatives.length
    ? alternatives.map((alt, idx) => {
      const caps = routeCapabilities(alt.capabilities);
      return `
      <div class="route-alt-row">
        <div><span class="badge">#${idx + 1}</span> <strong>${escapeHtml(routeValue(alt.target))}</strong></div>
        <div>${routeCostBadge(alt.cost_class)}</div>
        <div><code>${escapeHtml(routeValue(alt.mode))}</code>${alt.model ? `<div class="hint">${escapeHtml(alt.model)}</div>` : ''}</div>
        <div>${escapeHtml(routeValue(alt.reason))}<div class="hint">${escapeHtml(routeValue(alt.headroom))}</div>${caps ? `<div class="hint">${caps}</div>` : ''}</div>
      </div>`;
    }).join('')
    : '<div class="empty">No alternatives returned for this request.</div>';
  const headroomRows = Object.entries(byTarget);
  document.getElementById('route-headroom').innerHTML = headroomRows.length
    ? headroomRows.map(([target, info]) => `
      <div class="route-meta-box">
        <div class="label">${escapeHtml(target)}</div>
        <div class="value">${escapeHtml(routeValue(info.label))}</div>
        <div class="hint">used ${escapeHtml(routeValue(info.used))} · limit ${escapeHtml(routeValue(info.limit))} · remaining ${escapeHtml(routeValue(info.remaining))}</div>
      </div>`).join('')
    : '<div class="empty">No usage limits found.</div>';
  const sources = Array.isArray(usage.sources) ? usage.sources : [];
  document.getElementById('route-headroom-sources').textContent = sources.length
    ? `${sources.length.toLocaleString()} usage sources`
    : 'No usage sources';
}

document.getElementById('route-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const prompt = document.getElementById('route-prompt').value.trim();
  const prefer = document.getElementById('route-prefer').value;
  const capability = document.getElementById('route-capability').value;
  const status = document.getElementById('route-status');
  const btn = document.getElementById('route-submit');
  const params = new URLSearchParams({ prompt, prefer, capability });
  btn.disabled = true;
  status.textContent = 'Routing…';
  try {
    const r = await fetch(`/api/route?${params.toString()}`);
    const text = await r.text();
    let data = {};
    if (text) {
      try { data = JSON.parse(text); } catch (_) { data = { error: text }; }
    }
    if (!r.ok) {
      const msg = data.error || `HTTP ${r.status}`;
      status.innerHTML = `<span style="color:var(--err);">${escapeHtml(msg)}</span>`;
      document.getElementById('route-results').hidden = true;
      showToast(`Route preview failed: ${msg}`, 'err');
      return;
    }
    renderRouteResult(data);
    status.innerHTML = '<span class="badge ok">dry-run complete</span>';
    pushActivity(`route · ${data.chosen && data.chosen.target ? data.chosen.target : 'selected'}`);
  } catch (e) {
    status.innerHTML = `<span style="color:var(--err);">${escapeHtml(e.message || String(e))}</span>`;
    document.getElementById('route-results').hidden = true;
  } finally {
    btn.disabled = false;
  }
};

// Recall mode toggle (bm25 / hybrid).
(function wireRecallModeToggle() {
  const bm25Btn = document.getElementById('recall-mode-bm25');
  const hybridBtn = document.getElementById('recall-mode-hybrid');
  const val = document.getElementById('recall-mode-val');
  bm25Btn.onclick = () => {
    val.value = 'bm25';
    bm25Btn.classList.add('active');
    hybridBtn.classList.remove('active');
  };
  hybridBtn.onclick = () => {
    val.value = 'hybrid';
    hybridBtn.classList.add('active');
    bm25Btn.classList.remove('active');
  };
})();

// Memory
document.getElementById('recall-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const project = currentProject();
  if (isGlobalScope()) { showGlobalScopeEmpty('recall-results'); showToast(GLOBAL_SCOPE_MESSAGE, 'err'); return; }
  if (!project) { showToast('Select or add a project', 'err'); return; }
  const kindVal = document.getElementById('recall-kind').value;
  const compressedOnly = document.getElementById('recall-compressed-only').checked;
  const mode = document.getElementById('recall-mode-val').value || 'bm25';
  const body = {
    project,
    query: document.getElementById('recall-query').value,
    limit: Number(document.getElementById('recall-limit').value) || 10,
    filter: document.getElementById('recall-filter').value || null,
    kind: kindVal || null,
    compressed_only: compressedOnly || null,
    mode,
  };
  const results = document.getElementById('recall-results');
  results.innerHTML = `<div class="empty">Searching…</div>`;
  const r = await fetch('/api/memory/recall', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(body)});
  if (!r.ok) {
    const text = await r.text();
    showToast(`Memory search failed ${r.status}: ${text}`, 'err');
    results.innerHTML = `<div class="empty" style="color:var(--err);">${r.status}: ${text}</div>`;
    return;
  }
  const d = await r.json();
  // The backend returns the actual mode used (may differ from requested when embeddings are off).
  const usedMode = d.mode || mode;
  if (!d.hits.length) {
    results.innerHTML = `<div class="empty">No results. Try a different query or filter.</div>`;
    pushActivity(`recall ${body.project} · 0 hits (${usedMode})`);
    return;
  }
  results.innerHTML = d.hits.map(h => {
    const isCompressed = h.compressed;
    const compBadge = isCompressed
      ? `<span class="badge ok" title="Compressed">⊟ Compress</span>`
      : '';
    // Importance display — normalised via importancePct() for both 1-10 and 0-1 scales.
    let impBadge = '';
    if (h.importance !== undefined && h.importance !== null) {
      const pct = importancePct(h.importance);
      impBadge = `<span class="badge imp" title="importance ${pct}%">★ ${pct}%</span>`;
    }
    // Hybrid/semantic badge — shown when the result was surfaced by vector search.
    // backend mode values: "hybrid-vector" (Ollama) or "hybrid-graph" (BM25+graph fallback).
    const isVectorMode = usedMode === 'hybrid-vector';
    const isHybridMode = usedMode.startsWith('hybrid');
    const isSemanticHit = h.semantic || isVectorMode;
    const semanticLabel = isVectorMode ? '∼ vector' : isHybridMode ? '∼ hybrid' : '';
    const semanticBadge = isSemanticHit
      ? `<span class="badge semantic" title="Semantic search result (${escapeHtml(usedMode)})">${semanticLabel}</span>`
      : '';
    // Score display — right-aligned in meta row; vector scores are typically 0–1 floats.
    const scoreHtml = h.score !== undefined
      ? `<span class="rc-score">score <span class="score-val">${Number(h.score).toFixed(4)}</span></span>`
      : '';
    const origBlock = isCompressed && h.body_full
      ? `<details style="margin-top:0.4rem;">
           <summary style="cursor:pointer;color:var(--muted);font-size:0.82em;user-select:none;">View original (${h.body_full.length} chars)</summary>
           <div style="margin-top:0.4rem;font-size:0.85em;white-space:pre-wrap;overflow-wrap:anywhere;padding:0.5rem;background:var(--bg);border-radius:6px;border:1px solid var(--border);">${escapeHtml(h.body_full)}</div>
         </details>`
      : '';
    return `<div class="recall-card" style="cursor:pointer;" onclick="openDetailModal(${h.id})" title="View detail">
      <div class="rc-meta">
        <span class="badge">#${h.id}</span>
        <code style="font-size:0.82em;">${escapeHtml(h.kind || '?')}</code>
        ${compBadge}${semanticBadge}${impBadge}
        ${h.scope ? `<span style="font-size:0.8em;color:var(--muted);">${escapeHtml(h.scope)}</span>` : ''}
        ${scoreHtml}
      </div>
      <div class="rc-body">${escapeHtml(h.body || '')}</div>
      ${origBlock}
    </div>`;
  }).join('');
  pushActivity(`recall ${body.project} · ${d.hits.length} hits (${usedMode})`);
};

// Kick off a NON-BLOCKING embedding backfill and poll progress. The server
// returns immediately ({started}); a background thread embeds (minutes for 20k).
// We poll /api/memory/coverage and report N/M until it finishes, then reload the
// map if it's showing this project. `statusFn(text)` receives progress strings.
const EMBED_POLLING = new Set();
async function startEmbedAndPoll(project, statusFn) {
  if (!project) { showToast('Select a project first.', 'err'); return false; }
  let r;
  try {
    r = await fetch('/api/memory/embed', {
      method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ project }),
    });
  } catch (e) { showToast(`Network error: ${e.message || e}`, 'err'); return false; }
  if (r.status === 404) return false;                 // not compiled in
  if (!r.ok) { showToast(`Embedding error ${r.status}: ${await r.text()}`, 'err'); return false; }
  showToast('Embedding generation started (background) — usable while it runs', 'ok');
  pushActivity(`embed start · ${project}`);
  if (EMBED_POLLING.has(project)) return true;
  EMBED_POLLING.add(project);
  const poll = async () => {
    let c;
    try { c = await (await fetch(`/api/memory/coverage?project=${encodeURIComponent(project)}`)).json(); }
    catch (e) { EMBED_POLLING.delete(project); return; }
    const pct = c.total ? Math.round(100 * c.embedded / c.total) : 0;
    if (statusFn) statusFn(`Embeddings ${c.embedded.toLocaleString()}/${c.total.toLocaleString()} (${pct}%)${c.running ? ' · Generating…' : ' · done'}`);
    // keep the project cache fresh so the map basis badge updates
    const p = (PROJECTS_CACHE || []).find(x => x.name === project);
    if (p && !c.running && c.embedded > 0) p.embeddings_enabled = p.embeddings_enabled; // no-op, coverage drives basis
    if (c.running) { setTimeout(poll, 3000); return; }
    EMBED_POLLING.delete(project);
    showToast(`Embeddings done · ${project} · ${c.embedded.toLocaleString()}/${c.total.toLocaleString()}`, 'ok');
    if ((memmapProject || currentProject()) === project) loadMemmap(project);
  };
  poll();
  return true;
}

// Settings backfill + Generate buttons + the map button all share startEmbedAndPoll.
(function wireEmbedButtons() {
  const settingsBtns = ['embed-backfill-btn', 'cfg-emb-generate-btn'];
  const cap = document.getElementById('cfg-emb-coverage');
  settingsBtns.forEach(id => {
    const btn = document.getElementById(id);
    if (!btn) return;
    btn.onclick = async () => {
      const project = currentProject();
      if (isGlobalScope()) { showToast(GLOBAL_SCOPE_MESSAGE, 'err'); return; }
      await startEmbedAndPoll(project, (t) => { if (cap) cap.textContent = t; });
    };
  });
})();
document.getElementById('save-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const project = currentProject();
  if (isGlobalScope()) { document.getElementById('save-result').innerHTML = `<span style="color:var(--err);">${GLOBAL_SCOPE_MESSAGE}</span>`; return; }
  if (!project) { showToast('Select or add a project', 'err'); return; }
  let metadata = {};
  const raw = document.getElementById('save-metadata').value.trim();
  if (raw) { try { metadata = JSON.parse(raw); } catch (e) { document.getElementById('save-result').innerHTML = `<span style="color:var(--err);">JSON parse failed: ${e}</span>`; return; } }
  const body = {
    project,
    kind: document.getElementById('save-kind').value || 'note',
    body: document.getElementById('save-body').value,
    metadata,
  };
  const r = await fetch('/api/memory/save', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(body)});
  const out = document.getElementById('save-result');
  if (!r.ok) {
    const text = await r.text();
    showToast(`Memory Save failed ${r.status}: ${text}`, 'err');
    out.innerHTML = `<span style="color:var(--err);">${r.status}: ${text}</span>`;
    return;
  }
  const d = await r.json();
  out.innerHTML = `<span class="badge ok">✓ Save id=${d.id}</span>`;
  pushActivity(`save ${body.project} · id=${d.id}`);
};
document.getElementById('blocks-list-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const project = currentProject();
  if (isGlobalScope()) { document.querySelector('#blocks-tbl tbody').innerHTML = `<tr><td colspan="2" class="empty">${GLOBAL_SCOPE_MESSAGE}</td></tr>`; return; }
  if (!project) { showToast('Select or add a project', 'err'); return; }
  const tbody = document.querySelector('#blocks-tbl tbody');
  tbody.innerHTML = `<tr><td colspan="2" class="empty">Loading…</td></tr>`;
  const r = await fetch(`/api/memory/blocks?project=${encodeURIComponent(project)}`);
  if (!r.ok) {
    const text = await r.text();
    showToast(`Failed to load blocks ${r.status}: ${text}`, 'err');
    tbody.innerHTML = `<tr><td colspan="2" class="empty" style="color:var(--err);">${r.status}: ${text}</td></tr>`;
    return;
  }
  const d = await r.json();
  tbody.innerHTML = d.blocks.length
    ? d.blocks.map(b => `<tr><td><code>${b.kind.replace(/^block:/,'')}</code></td><td>${b.body.replace(/</g,'&lt;')}</td></tr>`).join('')
    : `<tr><td colspan="2" class="empty">No blocks. Add one below.</td></tr>`;
};
document.getElementById('blocks-set-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const project = currentProject();
  if (isGlobalScope()) { document.getElementById('block-set-result').innerHTML = `<span style="color:var(--err);">${GLOBAL_SCOPE_MESSAGE}</span>`; return; }
  if (!project) { showToast('Select or add a project', 'err'); return; }
  const body = {
    project,
    name: document.getElementById('block-set-name').value,
    body: document.getElementById('block-set-body').value,
  };
  const r = await fetch('/api/memory/blocks', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(body)});
  const out = document.getElementById('block-set-result');
  if (!r.ok) {
    const text = await r.text();
    showToast(`Blocks Save failed ${r.status}: ${text}`, 'err');
    out.innerHTML = `<span style="color:var(--err);">${r.status}: ${text}</span>`;
    return;
  }
  const d = await r.json();
  out.innerHTML = `<span class="badge ok">✓ Save id=${d.id}</span>`;
};
document.getElementById('export-form').onsubmit = (ev) => {
  ev.preventDefault();
  const project = currentProject();
  if (isGlobalScope()) { showToast(GLOBAL_SCOPE_MESSAGE, 'err'); return; }
  if (!project) { showToast('Select or add a project', 'err'); return; }
  window.location.href = `/api/memory/export?project=${encodeURIComponent(project)}`;
  pushActivity(`export ${project}`);
};

// Memory stats subtab — calls GET /api/memory/stats?project=
async function loadMemStats(project) {
  if (isGlobalScope() || isGlobalProjectValue(project)) {
    document.getElementById('memstats-loading').textContent = GLOBAL_SCOPE_MESSAGE;
    document.getElementById('memstats-loading').hidden = false;
    document.getElementById('memstats-body').hidden = true;
    return;
  }
  if (!project) return;
  const loading = document.getElementById('memstats-loading');
  const body = document.getElementById('memstats-body');
  loading.textContent = 'Stats Loading…';
  loading.hidden = false;
  body.hidden = true;
  let d;
  try {
    const r = await fetch(`/api/memory/stats?project=${encodeURIComponent(project)}`);
    if (!r.ok) {
      const text = await r.text();
      showToast(`Failed to load memory stats ${r.status}: ${text}`, 'err');
      loading.textContent = `Error ${r.status}: ${text}`;
      return;
    }
    d = await r.json();
  } catch (e) {
    loading.textContent = `Network error: ${e.message || e}`;
    return;
  }
  loading.hidden = true;
  body.hidden = false;

  // KPI tiles: total, compressed count, saved chars, compression ratio
  const ratio = d.total ? ((d.compressed_count || 0) / d.total * 100).toFixed(1) : '0';
  const savedK = (d.saved_chars || 0) >= 1000 ? `${((d.saved_chars || 0) / 1000).toFixed(1)}K` : String(d.saved_chars || 0);
  // saved_pct from server is the authoritative compression savings rate.
  const savedPctTile = d.saved_pct != null
    ? `<div class="kpi accent"><div class="label">Compression savings</div><div class="value">${Number(d.saved_pct).toFixed(1)}%</div><div class="sub">by chars</div></div>`
    : '';
  document.getElementById('memstats-kpis').innerHTML = `
    <div class="kpi accent"><div class="label">All Memory</div><div class="value">${(d.total || 0).toLocaleString()}</div><div class="sub">items</div></div>
    <div class="kpi"><div class="label">Compressed</div><div class="value">${(d.compressed_count || 0).toLocaleString()}</div><div class="sub">${ratio}%</div></div>
    <div class="kpi"><div class="label">Saved chars</div><div class="value">${savedK}</div><div class="sub">chars</div></div>
    ${savedPctTile}
  `;

  // Kind bars
  const byKind = d.by_kind || [];
  const maxCount = Math.max(1, ...byKind.map(k => k.count));
  document.getElementById('memstats-bars').innerHTML = byKind.length
    ? byKind.map(k => `
        <div class="stat-bar-row">
          <span style="overflow:hidden;text-overflow:ellipsis;white-space:nowrap;font-size:0.82em;">${escapeHtml(k.kind)}</span>
          <div class="stat-bar-bg"><div class="stat-bar-fill" style="width:${(k.count / maxCount * 100).toFixed(1)}%"></div></div>
          <span style="text-align:right;font-size:0.82em;font-variant-numeric:tabular-nums;">${k.count}</span>
        </div>`)
        .join('')
    : '<div class="empty">No kind data</div>';

  // Day trend sparkline — reuse existing spark() helper
  const byDay = d.by_day || [];
  spark('memstats-spark', byDay.map(x => x.count), 'var(--accent)');
}

document.getElementById('memstats-refresh').onclick = () => {
  const project = currentProject();
  if (project && !isGlobalScope()) { loadMemStats(project); loadQueue(project); }
};

// Compression queue — GET /api/memory/queue?project=
async function loadQueue(project) {
  if (isGlobalScope() || isGlobalProjectValue(project)) {
    document.getElementById('queue-summary').textContent = GLOBAL_SCOPE_MESSAGE;
    document.getElementById('queue-list').innerHTML = '';
    return;
  }
  if (!project) return;
  const summary = document.getElementById('queue-summary');
  const list = document.getElementById('queue-list');
  let d;
  try {
    const r = await fetch(`/api/memory/queue?project=${encodeURIComponent(project)}`);
    if (!r.ok) { showToast(`Failed to load compression queue ${r.status}`, 'err'); summary.textContent = `Error ${r.status}`; return; }
    d = await r.json();
  } catch (e) { summary.textContent = `Network error: ${e.message || e}`; return; }
  const onoff = d.enabled ? 'auto-compress ON' : 'auto-compress OFF (enable in Config)';
  summary.innerHTML = `queue ${d.ready + d.waiting} · ready <b>${d.ready}</b> · aging ${d.waiting} `
    + `· Model <code>${escapeHtml(d.model)}</code> · ≥${d.min_chars} chars · ${onoff}`;
  if (!d.items.length) {
    list.innerHTML = '<div class="empty">Queue empty — no compression candidates (long uncompressed memories).</div>';
    return;
  }
  list.innerHTML = d.items.map(i => {
    // saved_pct is present on already-compressed rows; null/absent means pending.
    const pctBadge = i.saved_pct != null
      ? `<span class="badge save">−${Number(i.saved_pct).toFixed(1)}%</span>`
      : `<span style="font-size:0.78em;color:var(--muted);">queued</span>`;
    return `<div class="hist-item">
      <span class="kind">${escapeHtml(i.kind)}</span>
      <span class="body">#${i.id} · ${i.chars} chars · ${i.age_min}m ago</span>
      ${pctBadge}
      <span class="kind" style="color:${i.ready ? 'var(--ok)' : 'var(--muted)'}">${i.ready ? '✓ ready' : '⏳ aging'}</span>
    </div>`;
  }).join('');
}

// Governance subtab logic.
let GOV_PREVIEW_IDS = [];

async function loadGovStats(project) {
  if (isGlobalScope() || isGlobalProjectValue(project)) return;
  if (!project) return;
  // Reuse the existing stats endpoint to populate the summary boxes.
  try {
    const r = await fetch(`/api/memory/stats?project=${encodeURIComponent(project)}`);
    if (!r.ok) return;
    const d = await r.json();
    document.getElementById('gov-total').textContent = (d.total || 0).toLocaleString();
    const comp = d.compressed_count || 0;
    const uncomp = (d.total || 0) - comp;
    document.getElementById('gov-compressed').textContent = comp.toLocaleString();
    document.getElementById('gov-uncompressed').textContent = uncomp.toLocaleString();
    const savedK = (d.saved_chars || 0) >= 1000
      ? `${((d.saved_chars || 0) / 1000).toFixed(1)}K`
      : String(d.saved_chars || 0);
    document.getElementById('gov-saved').textContent = savedK;
  } catch (_) { /* stats API optional */ }
}

async function runGovPreview() {
  const project = currentProject();
  if (isGlobalScope()) { showGlobalScopeEmpty('gov-preview-result'); return; }
  if (!project) return;
  const kind = document.getElementById('gov-kind').value;
  const compFilter = document.getElementById('gov-compress-filter').value;
  const before = document.getElementById('gov-before').value;
  const after = document.getElementById('gov-after').value;

  const params = new URLSearchParams({ project, limit: 200, sort: 'recent' });
  if (kind) params.set('kind', kind);
  if (compFilter === 'compressed') params.set('compressed_only', '1');
  if (before) params.set('before', Math.floor(new Date(before).getTime() / 1000));
  if (after) params.set('after', Math.floor(new Date(after).getTime() / 1000));

  const previewEl = document.getElementById('gov-preview-result');
  const dangerZone = document.getElementById('gov-danger-zone');
  previewEl.innerHTML = '<div class="empty">Loading…</div>';
  dangerZone.hidden = true;
  GOV_PREVIEW_IDS = [];

  let d;
  try {
    const r = await fetch(`/api/memory/timeline?${params}`);
    if (r.status === 404) {
      previewEl.innerHTML = '<div class="empty" style="color:var(--muted);">Timeline API not supported (404)</div>';
      return;
    }
    if (!r.ok) { previewEl.innerHTML = `<div class="empty" style="color:var(--err);">Error ${r.status}</div>`; return; }
    d = await r.json();
  } catch (e) {
    previewEl.innerHTML = `<div class="empty" style="color:var(--err);">Network error: ${e.message || e}</div>`;
    return;
  }

  // Client-side filtering for compressed_only=uncompressed (server may not support it).
  let items = d.items || [];
  if (compFilter === 'uncompressed') items = items.filter(i => !i.compressed);
  if (compFilter === 'compressed') items = items.filter(i => i.compressed);

  GOV_PREVIEW_IDS = items.map(i => i.id);

  if (!items.length) {
    previewEl.innerHTML = '<div class="empty">No matching items.</div>';
    dangerZone.hidden = true;
    return;
  }

  previewEl.innerHTML = items.slice(0, 50).map(i => {
    const compBadge = i.compressed ? `<span style="color:var(--ok);font-size:0.78em;">⊟</span>` : '';
    const snippet = (i.body || '').slice(0, 80).replace(/\n/g, ' ');
    return `<div class="hist-item" style="cursor:default;">
      <span class="when">${relativeTime(i.created_at)}</span>
      <span class="kind">${escapeHtml(i.kind)}</span>${compBadge}
      <span class="body" style="flex:1;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;">${escapeHtml(snippet)}${i.body && i.body.length > 80 ? '…' : ''}</span>
    </div>`;
  }).join('') + (items.length > 50 ? `<div class="empty" style="font-size:0.82em;">… and ${items.length - 50} more (preview caps at 50)</div>` : '');

  document.getElementById('gov-match-count').textContent = `${items.length} match`;
  dangerZone.hidden = false;
  document.getElementById('gov-delete-result').textContent = '';
}

document.getElementById('gov-preview-btn').onclick = runGovPreview;
document.getElementById('gov-refresh').onclick = () => {
  const project = currentProject();
  if (project && !isGlobalScope()) { loadGovStats(project); runGovPreview(); }
};

document.getElementById('gov-bulk-delete-btn').onclick = async () => {
  if (isGlobalScope()) { showToast(GLOBAL_SCOPE_MESSAGE, 'err'); return; }
  if (!GOV_PREVIEW_IDS.length) return;
  const ok = confirm(`Delete all ${GOV_PREVIEW_IDS.length} items matching the filter. This action cannot be undone.`);
  if (!ok) return;
  const resultEl = document.getElementById('gov-delete-result');
  resultEl.textContent = 'Deleting…';
  try {
    const r = await fetch('/api/memory/delete', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ ids: GOV_PREVIEW_IDS }),
    });
    if (r.status === 404) {
      resultEl.innerHTML = '<span style="color:var(--muted);">Delete API not supported (404)</span>';
      return;
    }
    if (!r.ok) { resultEl.innerHTML = `<span style="color:var(--err);">Error ${r.status}</span>`; return; }
    const d = await r.json();
    resultEl.innerHTML = `<span class="badge ok">✓ ${d.deleted} deleted</span>`;
    pushActivity(`Manage delete · ${d.deleted}`);
    GOV_PREVIEW_IDS = [];
    document.getElementById('gov-preview-result').innerHTML = '<div class="empty">Deleted. Run the preview again.</div>';
    document.getElementById('gov-danger-zone').hidden = true;
    const project = currentProject();
    if (project && !isGlobalScope()) loadGovStats(project);
  } catch (e) {
    resultEl.innerHTML = `<span style="color:var(--err);">Error: ${e.message || e}</span>`;
  }
};

document.getElementById('queue-compress-all').onclick = async (ev) => {
  const project = currentProject();
  if (isGlobalScope()) { showToast(GLOBAL_SCOPE_MESSAGE, 'err'); return; }
  if (!project) return;
  const btn = ev.currentTarget;
  btn.disabled = true; btn.textContent = 'Compressing…';
  try {
    const r = await fetch('/api/memory/compress', {
      method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ project }),
    });
    const d = await r.json();
    pushActivity(`Compress ${project}: ${d.compressed} (skip ${d.skipped})`);
  } catch (e) { pushActivity(`Compress error: ${e.message || e}`); }
  btn.disabled = false; btn.textContent = 'Compress all now';
  loadQueue(project); loadMemStats(project);
};

// Memory knowledge map — Cytoscape.js node graph (library owns layout/zoom/pan/drag).
// Drives the #sub-memmap subtab using the server LOD API:
//   GET /api/memory/graph?project=X&mode=overview  -> { clusters, cluster_edges, total_nodes, ... }
//   GET /api/memory/graph?project=X&cluster=<id>   -> { root, nodes, edges }
// We never hand-roll a force simulation: Cytoscape's built-in 'cose' layout
// positions nodes, and the cy instance handles wheel-zoom / background-pan /
// node-drag natively. If the CDN failed to load (offline), window.cytoscape is
// undefined and we fall back to a plain list instead of crashing.
let memmapOverview = null;           // last overview payload for the current project
let memmapProject = '';              // project the overview belongs to
let memmapSource = 'all';            // 'all' | 'main' | 'subagent'
let memmapSearch = '';               // cluster label filter substring
let memmapGroup = 'context';         // grouping basis: context|file|kind|session|source|time
let memmapBasis = 'auto';            // context classification basis: auto|vector(semantic)|lexical(lexical)
let memmapTarget = 320;              // granularity: overview bubble target
let memmapDepth = 0;                 // depth: drill leaf cutoff (0 = auto)
let memmapMode = 'overview';         // 'overview' | 'group' | 'leaf' (current level kind)
let memmapCy = null;                 // the single Cytoscape instance, lazily created
let memmapLayout = null;             // the running layout (cola is continuous; stop before re-running)
// map mode: 'brain' (concept graph, default) vs 'cluster' (legacy bundle view). The toggle swaps
// which loader runs; the cy instance, styling, layout + tooltip are shared.
let memmapViewMode = 'brain';
let brainGraph = null;               // last brain TOP-LEVEL (communities) payload for the scope
let brainProject = '';               // project the brain belongs to ('' = global)
let brainScopeGlobal = false;        // whether the loaded brain is the GLOBAL (merged) brain
let brainConcept = null;             // when set, we're viewing one concept's memories (drill)
// Brain is now 3-level: communities → concepts → memories. brainLevel tracks the
// current depth; brainStack is the breadcrumb of pushed levels (community, then
// concept) — the communities overview is the implicit "All(topics)" root.
//   { kind:'community', id, label }  — a topic community we drilled into
//   { kind:'concept',   token, label } — a concept we drilled into
let brainLevel = 'community';        // 'community' | 'concept' | 'memory'
let brainStack = [];
let brainCommunity = null;           // the community we're inside, when at concept/memory level
// Cache concept sub-graphs per community id so breadcrumb pops re-render instantly.
const brainCommunityCache = new Map();
// Breadcrumb stack of drilled levels: [{ token, label }, …]. The overview itself
// is the implicit root (rendered as the "All" crumb); pushed levels follow.
let memmapStack = [];
// Per-level cache keyed by drill token -> the raw level payload (group or leaf).
// Lets breadcrumb pops / re-opens render instantly without re-fetching.
const memmapLevelCache = new Map();

const MEMMAP_GROUP_KO = {
  context: 'Context', file: 'File', kind: 'Kind', session: 'Session', source: 'Source', time: 'Time',
};

function memmapSourceLabelKo(source) {
  return source === 'subagent' ? '🤖Subagent' : '🧠Main';
}

// True only when the Cytoscape CDN script actually loaded.
function memmapHasCy() {
  return typeof window !== 'undefined' && typeof window.cytoscape === 'function';
}

// Flag the catch-all/misc cluster: the lone bucket that is huge versus the rest
// (>= 8x the median size). Returns its id, or null when no cluster dominates.
function memmapCatchAllId(clusters) {
  if (clusters.length < 3) return null;
  const sizes = clusters.map(c => Number(c.size || 0)).sort((a, b) => a - b);
  const median = sizes[Math.floor(sizes.length / 2)] || 1;
  let biggest = null;
  for (const c of clusters) {
    if (!biggest || Number(c.size || 0) > Number(biggest.size || 0)) biggest = c;
  }
  if (biggest && Number(biggest.size || 0) >= median * 8) return biggest.id;
  return null;
}

// Small, purposeful palette keyed to MEANING (source), not a per-id hash —
// Memory map gold/blue/green. Main = blue, subagent = gold, mixed = green,
// catch-all/misc = neutral grey. This is what makes the map read as clean +
// structured instead of an 18-colour rainbow.
const MEMMAP_PALETTE = {
  main: '#1D4E89',      // deep blue
  subagent: '#B8860B',  // gold
  mixed: '#2D6A4F',     // green
  misc: '#9CA3AF',      // neutral grey (catch-all)
};
// Colour a node by its source bucket ('main' | 'subagent' | 'mixed').
function memmapColor(source) {
  return MEMMAP_PALETTE[String(source)] || MEMMAP_PALETTE.mixed;
}

function memmapCyStyle() {
  return [
    {
      selector: 'node',
      style: {
        'background-color': 'data(col)',
        'background-opacity': 0.92,
        'shape': 'ellipse',
        'label': 'data(label)',
        'width': 'data(size)',
        'height': 'data(size)',
        'font-size': '10px',
        'font-family': 'var(--font-ui, sans-serif)',
        'color': '#e6edf3',          // light label text on the dark canvas
        'text-valign': 'bottom',
        'text-halign': 'center',
        'text-margin-y': 4,
        'text-wrap': 'ellipsis',
        'text-max-width': '130px',
        // Dark halo keeps labels readable over the glowing nodes/grid.
        'text-outline-width': 2,
        'text-outline-color': '#0b0e14',
        'text-outline-opacity': 0.9,
        // Soft same-colour glow behind each node = the neural-constellation look.
        'underlay-color': 'data(col)',
        'underlay-opacity': 0.28,
        'underlay-padding': 5,
        'underlay-shape': 'ellipse',
        'border-width': 1.5,
        'border-color': 'rgba(255,255,255,0.22)',
        'min-zoomed-font-size': 8,
      },
    },
    // SHAPES carry meaning: cluster/group bubbles are ellipse
    // hubs; leaf member nodes are round-rectangles ("things" hanging off a hub).
    { selector: 'node[kindType = "cluster"]', style: { 'shape': 'ellipse' } },
    { selector: 'node[kindType = "member"]', style: { 'shape': 'round-rectangle' } },
    // Brain concept nodes are circular "ideas"; labels are hidden by default
    // (only hubs + hover show them) so the map reads as a clean constellation.
    {
      selector: 'node[kindType = "concept"]',
      style: { 'shape': 'ellipse', 'text-opacity': 0 },
    },
    // Hubs (high degree) keep their label so the brain has anchor points.
    { selector: 'node[kindType = "concept"][?hub]', style: { 'text-opacity': 1 } },
    // TOPIC COMMUNITIES (top brain level): hexagon super-nodes so topics read
    // differently from the round concept "ideas". Labels always on (only a few
    // dozen of them) with a brighter border to mark them as a level above.
    {
      selector: 'node[kindType = "community"]',
      style: {
        'shape': 'hexagon',
        'text-opacity': 1,
        'font-size': '12px',
        'border-width': 2,
        'border-color': 'rgba(255,255,255,0.40)',
        'underlay-opacity': 0.32,
      },
    },
    // Hovered concept (+ its neighbours) always shows its label.
    { selector: 'node.label-on', style: { 'text-opacity': 1 } },
    // Catch-all / misc bubble: neutral, dashed, visually set apart from real groups.
    {
      selector: 'node[?misc]',
      style: { 'border-style': 'dashed', 'border-color': 'rgba(17,17,17,0.20)', 'background-opacity': 0.45 },
    },
    {
      selector: 'node.dim',          // hidden by source filter / search miss
      style: { 'opacity': 0.12, 'text-opacity': 0.06 },
    },
    {
      selector: 'node.hit',          // search match highlight (red ring)
      style: { 'border-width': 3, 'border-color': '#CC0000' },
    },
    {
      selector: 'edge',
      style: {
        'width': 'mapData(w, 1, 4, 1, 2.5)',
        'line-color': '#C8C8C2',     // thin light grey
        'opacity': 0.45,
        'curve-style': 'straight',   // repositions live with the continuous sim
      },
    },
    // Hover / selection: soft accent glow only here (not a permanent halo).
    {
      selector: 'node:selected',
      style: {
        'border-width': 3, 'border-color': '#1D4E89',
        'underlay-color': '#1D4E89', 'underlay-opacity': 0.18,
        'underlay-padding': 6, 'underlay-shape': 'ellipse',
      },
    },
    // Edges touching a hovered/selected node pop so connections read clearly.
    {
      selector: 'edge.hl',
      style: { 'line-color': '#1D4E89', 'opacity': 0.85, 'width': 'mapData(w, 1, 4, 2, 4)', 'z-index': 9 },
    },
  ];
}

// Create (once) or return the shared Cytoscape instance bound to #cy.
let memmapFcoseReady = false;
function memmapRegisterFcose() {
  if (memmapFcoseReady) return true;
  if (!memmapHasCy() || typeof window.cytoscapeFcose !== 'function') return false;
  try { window.cytoscape.use(window.cytoscapeFcose); memmapFcoseReady = true; }
  catch (_) { memmapFcoseReady = false; }
  return memmapFcoseReady;
}

let memmapColaReady = false;
function memmapRegisterCola() {
  if (memmapColaReady) return true;
  if (!memmapHasCy() || typeof window.cytoscapeCola !== 'function') return false;
  try { window.cytoscape.use(window.cytoscapeCola); memmapColaReady = true; }
  catch (_) { memmapColaReady = false; }
  return memmapColaReady;
}

function memmapEnsureCy() {
  if (memmapCy) return memmapCy;
  if (!memmapHasCy()) return null;
  memmapRegisterFcose();
  const container = document.getElementById('cy');
  if (!container) return null;
  memmapCy = window.cytoscape({
    container,
    style: memmapCyStyle(),
    minZoom: 0.1,
    maxZoom: 3,
    wheelSensitivity: 0.2,
  });
  // Tap on a node: a bubble (cluster) drills via its token; a member opens the
  // modal; a concept shows its memories.
  memmapCy.on('tap', 'node', (ev) => {
    const data = ev.target.data();
    if (data.kindType === 'community') {
      brainDrillCommunity(data.communityId, String(data.rawLabel || data.label || ''));
      return;
    }
    if (data.kindType === 'concept') {
      brainDrillConcept(String(data.token || data.label || ''));
      return;
    }
    if (data.kindType === 'cluster') {
      // Not drillable (misc/leaf-of-leaf): just centre it, don't try to drill.
      if (data.drillable === false || data.misc || !data.token) {
        try { memmapCy.animate({ center: { eles: ev.target }, duration: 200 }); } catch (_) { /* ignore */ }
        return;
      }
      memmapDrill(String(data.token), String(data.rawLabel || data.token));
    } else if (data.kindType === 'member') {
      const num = String(data.memId || '').replace(/^m/, '');
      if (/^\d+$/.test(num)) openDetailModal(Number(num));
    }
  });
  // Hover a node: light up its connecting edges + show the Memory card.
  // In brain mode, fade the non-neighbours and reveal the hovered node + its
  // neighbours' labels (Memory map focus behaviour).
  memmapCy.on('mouseover', 'node', (ev) => {
    ev.target.connectedEdges().addClass('hl');
    if (memmapViewMode === 'brain' && !brainConcept) {
      const focus = ev.target.closedNeighborhood();
      memmapCy.nodes().not(focus).addClass('dim');
      focus.nodes().addClass('label-on');
    }
    memmapShowTooltip(ev.target);
  });
  memmapCy.on('mouseout', 'node', () => {
    memmapCy.edges('.hl').removeClass('hl');
    if (memmapViewMode === 'brain' && !brainConcept) {
      memmapCy.nodes().removeClass('dim label-on');
    }
    memmapHideTooltip();
  });
  // Track the cursor so the card follows it.
  memmapCy.on('mousemove', 'node', (ev) => memmapMoveTooltip(ev));
  // Any pan/zoom/tap-on-background dismisses the card so it never sticks.
  memmapCy.on('pan zoom', () => memmapHideTooltip());
  memmapCy.on('tap', (ev) => { if (ev.target === memmapCy) memmapHideTooltip(); });
  return memmapCy;
}

// Hover card: "<name> · <TYPE> · N connections". TYPE is
// derived from kindType + misc (cluster / memory / unclassified); connection count is the
// live cy node degree. The card is a DOM div positioned over the .memmap-cy-wrap.
function memmapTooltipMeta(node) {
  const data = node.data();
  if (data.misc) return { type: 'Unclassified', col: MEMMAP_PALETTE.misc };
  if (data.kindType === 'community') return { type: 'Topic', col: data.col || MEMMAP_PALETTE.mixed };
  if (data.kindType === 'concept') return { type: 'Concept', col: data.col || MEMMAP_PALETTE.mixed };
  if (data.kindType === 'member') return { type: 'Memory', col: memmapColor(data.src || 'mixed') };
  return { type: 'Cluster', col: memmapColor(data.src || 'mixed') };
}
function memmapShowTooltip(node) {
  const tip = document.getElementById('memmap-tooltip');
  if (!tip) return;
  const data = node.data();
  const meta = memmapTooltipMeta(node);
  const name = String(data.rawLabel || data.label || data.id || '');
  const conns = node.degree(false);   // edges touching this node (live)
  tip.innerHTML = '';
  const nm = document.createElement('div');
  nm.className = 'tt-name';
  nm.textContent = name || '(no name)';
  const ty = document.createElement('div');
  ty.className = 'tt-type';
  ty.textContent = meta.type;
  ty.style.backgroundColor = meta.col;
  const cn = document.createElement('div');
  cn.className = 'tt-conns';
  if (data.kindType === 'community') {
    // Topic super-node: how many concepts it groups + a few representative ones.
    const tops = Array.isArray(data.topConcepts) ? data.topConcepts : [];
    const cnt = Number(data.conceptCount || 0);
    cn.textContent = `${cnt.toLocaleString()} concepts` + (tops.length ? ` · ${tops.join(', ')}` : '');
  } else {
    cn.textContent = `${conns} connections`;
  }
  tip.appendChild(nm);
  tip.appendChild(ty);
  tip.appendChild(cn);
  tip.classList.add('visible');
}
function memmapMoveTooltip(ev) {
  const tip = document.getElementById('memmap-tooltip');
  if (!tip || !ev.renderedPosition) return;
  // renderedPosition is relative to the cy canvas, which fills .memmap-cy-wrap.
  tip.style.left = `${Math.round(ev.renderedPosition.x) + 12}px`;
  tip.style.top = `${Math.round(ev.renderedPosition.y) + 12}px`;
}
function memmapHideTooltip() {
  const tip = document.getElementById('memmap-tooltip');
  if (tip) tip.classList.remove('visible');
}

// Set the current level kind and toggle controls. 'overview'/'group' are bubble
// levels (source filter + search apply); 'leaf' shows individual members (hidden).
function memmapSetMode(mode) {
  memmapMode = mode;
  const isBubble = mode === 'overview' || mode === 'group';
  const back = document.getElementById('memmap-back');
  if (back) back.hidden = memmapStack.length === 0;   // hidden at the root
  // Source filter + search only apply to bubble levels (overview / group).
  const filter = document.getElementById('memmap-source-filter');
  if (filter) filter.style.display = isBubble ? '' : 'none';
  const search = document.getElementById('memmap-search');
  if (search) search.style.display = isBubble ? '' : 'none';
}

// Render the breadcrumb bar: "All › label › label". Each non-current crumb pops back
// to that depth and re-renders from cache. Root crumb (All) reloads the overview.
function memmapRenderCrumbs() {
  const bar = document.getElementById('memmap-crumbs');
  if (!bar) return;
  bar.innerHTML = '';
  const crumbs = [{ token: null, label: 'All' }].concat(memmapStack);
  crumbs.forEach((c, i) => {
    if (i > 0) {
      const sep = document.createElement('span');
      sep.className = 'crumb-sep';
      sep.textContent = '›';
      bar.appendChild(sep);
    }
    const isLast = i === crumbs.length - 1;
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'crumb' + (isLast ? ' current' : '');
    btn.textContent = c.label;
    if (!isLast) btn.onclick = () => memmapGoToDepth(i);   // i === stack index to keep
    bar.appendChild(btn);
  });
  bar.hidden = false;
}

// Clear the offline/no-cy fallback list + empty notice + the cy container.
function memmapResetViews() {
  memmapStopLayout();   // halt the continuous sim before nodes get removed/replaced
  const fb = document.getElementById('memmap-fallback');
  if (fb) { fb.hidden = true; fb.innerHTML = ''; }
  const empty = document.getElementById('memmap-empty');
  if (empty) empty.hidden = true;
}

// Thin a dense edge set down to a "backbone": keep, for every node, only its
// single strongest edge (dedup by edge). A full 2000-edge / 4000-edge set forms a
// hairball that the force layout collapses into overlapping blobs; the backbone
// keeps the map connected without the clutter.
function memmapBackboneEdges(rawEdges, validIds) {
  const best = new Map();   // nodeId -> {src,dst,w}
  for (const e of rawEdges) {
    const s = String(e.src), d = String(e.dst);
    if (!validIds.has(s) || !validIds.has(d) || s === d) continue;
    const w = Number(e.weight || 0);
    const bs = best.get(s);
    if (!bs || w > bs.w) best.set(s, { src: s, dst: d, w });
    const bd = best.get(d);
    if (!bd || w > bd.w) best.set(d, { src: s, dst: d, w });
  }
  const seen = new Set(), out = [];
  for (const e of best.values()) {
    const key = e.src < e.dst ? `${e.src}|${e.dst}` : `${e.dst}|${e.src}`;
    if (seen.has(key)) continue;
    seen.add(key);
    out.push(e);
  }
  return out;
}

// Build the Cytoscape bubble elements from a clusters[] + cluster_edges[] pair.
// Used by both the overview and every drilled "group" level (same bubble shape).
// Each bubble carries `token` + `drillable` so the tap handler can drill deeper.
function memmapOverviewElements(payload) {
  payload = payload || memmapOverview || {};
  const clusters = Array.isArray(payload.clusters) ? payload.clusters : [];
  const edges = Array.isArray(payload.cluster_edges) ? payload.cluster_edges : [];
  const catchAllId = memmapCatchAllId(clusters);
  // Size scaling: clamp node diameter so a few hubs read clearly while satellites
  // stay small (hub-and-spoke). Tighter range than before keeps the map airy.
  const sizes = clusters.map(c => Number(c.size || 0));
  const maxSize = Math.max(1, ...sizes);
  const SCALE_MIN = 18, SCALE_MAX = 58;
  const ids = new Set();
  const nodes = clusters.map(c => {
    const id = String(c.id);
    ids.add(id);
    const size = Number(c.size || 0);
    const isMisc = c.id === catchAllId;
    const dom = c.dominant_source || 'mixed';
    // sqrt scale keeps small clusters visible; misc forced small.
    let diam = SCALE_MIN + (SCALE_MAX - SCALE_MIN) * Math.sqrt(size / maxSize);
    if (isMisc) diam = SCALE_MIN;
    const label = isMisc
      ? `Unclassified (${size.toLocaleString()})`
      : `${String(c.label || '(no name)')} (${size.toLocaleString()})`;
    return {
      data: {
        id, label, size: Math.round(diam), src: dom,
        // Colour by MEANING (source), neutral grey for the catch-all.
        col: isMisc ? memmapColor('misc') : memmapColor(dom),
        misc: isMisc ? 1 : undefined,
        kindType: 'cluster', clusterId: id, dom,
        token: c.token != null ? String(c.token) : '',
        drillable: c.drillable !== false,
        rawLabel: String(c.label || ''),
      },
    };
  });
  const edgeEls = memmapBackboneEdges(edges, ids)
    .map((e, i) => ({
      data: {
        id: `ce${i}`,
        source: String(e.src),
        target: String(e.dst),
        w: Math.max(1, Math.min(4, Number(e.w || 1))),
      },
    }));
  return nodes.concat(edgeEls);
}

// Build the Cytoscape elements for one cluster's members (drill-down).
function memmapMemberElements(payload) {
  const members = (payload.nodes || []).filter(
    n => n.node_type === 'memory' || n.node_type === undefined,
  );
  const ids = new Set(members.map(n => String(n.id)));
  const nodes = members.map(n => {
    const source = n.source_kind || 'main';
    const preview = String(n.label || n.preview || n.body || n.id || '');
    return {
      data: {
        id: String(n.id), label: preview, size: 24, src: source,
        // Member colour also keyed to source (main/subagent/mixed), not a hash.
        col: memmapColor(source),
        kindType: 'member', memId: String(n.id || ''),
        rawLabel: preview,
      },
    };
  });
  const edges = memmapBackboneEdges(payload.edges || [], ids)
    .map((e, i) => ({
      data: {
        id: `me${i}`,
        source: String(e.src),
        target: String(e.dst),
        w: Math.max(1, Math.min(3, Number(e.w || 1))),
      },
    }));
  return nodes.concat(edges);
}

// Stop the running layout (cola runs continuously; must be stopped before a
// re-layout or when the user leaves the map tab).
function memmapStopLayout() {
  if (memmapLayout) { try { memmapLayout.stop(); } catch (_) { /* ignore */ } memmapLayout = null; }
}

// Run the layout. Prefers cola — a CONTINUOUS force sim, so dragging a node makes
// its neighbours spring and the rest avoid in real time (the live motion the
// static one-shot layouts lacked). Degrades to fcose -> cose -> concentric if the
// extension fails to load.
function memmapRunLayout() {
  if (!memmapCy) return;
  // Re-sync Cytoscape to the live container size first. Drilling shows/hides the
  // breadcrumb, which changes #cy's height; without resize() the internal
  // viewport stays stale and click hit-testing lands on the wrong coordinates.
  try { memmapCy.resize(); } catch (_) { /* ignore */ }
  const n = memmapCy.nodes().length;
  memmapStopLayout();
  if (memmapRegisterCola()) {
    try {
      const layout = memmapCy.layout({
        name: 'cola',
        animate: true,            // animate every tick (the live motion)
        infinite: true,           // keep simulating so drags reflow neighbours
        fit: false,               // we fit once on first settle, not every tick
        refresh: 1,
        maxSimulationTime: 4000,
        nodeSpacing: 16,          // min gap around each node (overlap avoidance)
        edgeLength: 130,
        avoidOverlap: true,
        handleDisconnected: true, // arrange disconnected islands instead of stacking
        randomize: false,
        unconstrIter: 10,
        userConstIter: 10,
      });
      memmapLayout = layout;
      layout.one('layoutready', () => { try { memmapCy.fit(undefined, 36); } catch (_) { /* ignore */ } });
      layout.run();
      return;
    } catch (_) { memmapStopLayout(); /* fall through */ }
  }
  if (memmapRegisterFcose()) {
    try {
      memmapCy.layout({
        name: 'fcose', quality: 'default', randomize: true, animate: true,
        animationDuration: 600, packComponents: true, nodeRepulsion: 9000,
        idealEdgeLength: 120, nodeSeparation: 90, gravity: 0.2, gravityRange: 3.0,
        numIter: n > 600 ? 1500 : 2500, fit: true, padding: 36,
      }).run();
      return;
    } catch (_) { /* fall through to cose */ }
  }
  try {
    memmapCy.layout({
      name: 'cose', animate: true, nodeRepulsion: 12000, nodeOverlap: 24,
      componentSpacing: 140, idealEdgeLength: 120, gravity: 0.2, fit: true, padding: 36,
    }).run();
  } catch (_) {
    try { memmapCy.layout({ name: 'concentric', fit: true }).run(); } catch (_e) { /* ignore */ }
  }
}

// Render the overview cluster graph (or fallback list when cy is unavailable).
// The overview is the breadcrumb root: it clears the drill stack.
function renderMemmapOverview() {
  const head = document.getElementById('memmap-overview-head');
  memmapStack = [];
  memmapSetMode('overview');
  memmapRenderCrumbs();
  memmapResetViews();
  if (!memmapOverview) {
    if (head) head.textContent = '';
    if (memmapCy) memmapCy.elements().remove();
    return;
  }
  const clusters = Array.isArray(memmapOverview.clusters) ? memmapOverview.clusters : [];
  const total = Number(memmapOverview.total_nodes || 0);
  const groupKo = MEMMAP_GROUP_KO[memmapGroup] || memmapGroup;
  let caption = `All ${total.toLocaleString()} · ${clusters.length.toLocaleString()} groups · basis: ${groupKo}`;
  // For the context basis, show whether the map is using the semantic
  // (embedding/vector) signal or the keyword (lexical) fallback, plus coverage.
  if (memmapGroup === 'context') {
    const emb = Number(memmapOverview.embedded || 0);
    const totRows = Number(memmapOverview.total_rows || 0);
    const cov = totRows ? ` (Embeddings ${emb.toLocaleString()}/${totRows.toLocaleString()})` : '';
    caption += memmapOverview.basis === 'vector'
      ? ` · 🧠 semantic${cov}`
      : ` · 🔤 lexical${cov}`;
  }
  if (head) head.textContent = caption;
  if (!clusters.length) {
    const empty = document.getElementById('memmap-empty');
    if (empty) empty.hidden = false;
    if (memmapCy) memmapCy.elements().remove();
    return;
  }
  const cy = memmapEnsureCy();
  if (!cy) { renderMemmapFallback(clusters); return; }
  cy.elements().remove();
  cy.add(memmapOverviewElements(memmapOverview));
  memmapApplyFilters();
  memmapRunLayout();
}

// Render a drilled "group" level: bubbles again (same builder), with a caption and
// the source filter/search re-enabled. Assumes the crumb is already on the stack.
function memmapRenderGroupLevel(payload, label) {
  const head = document.getElementById('memmap-overview-head');
  memmapSetMode('group');
  memmapRenderCrumbs();
  memmapResetViews();
  const clusters = Array.isArray(payload.clusters) ? payload.clusters : [];
  const total = Number(payload.total_nodes || 0);
  if (head) head.textContent =
    `${label} · ${total.toLocaleString()} · ${clusters.length.toLocaleString()} groups`;
  if (!clusters.length) {
    const empty = document.getElementById('memmap-empty');
    if (empty) empty.hidden = false;
    if (memmapCy) memmapCy.elements().remove();
    return;
  }
  const cy = memmapEnsureCy();
  if (!cy) { renderMemmapFallback(clusters); return; }
  cy.elements().remove();
  cy.add(memmapOverviewElements(payload));
  memmapApplyFilters();
  memmapRunLayout();
}

// Render a "leaf" level: individual member nodes; tapping one opens the modal.
function memmapRenderLeafLevel(payload, label) {
  const head = document.getElementById('memmap-overview-head');
  memmapSetMode('leaf');
  memmapRenderCrumbs();
  memmapResetViews();
  const memberCount = (payload.nodes || []).filter(
    n => n.node_type === 'memory' || n.node_type === undefined,
  ).length;
  if (head) head.textContent = `${label} · ${memberCount.toLocaleString()} memories`;
  const cy = memmapEnsureCy();
  if (!cy) { renderMemmapMemberFallback(payload); return; }
  cy.elements().remove();
  cy.add(memmapMemberElements(payload));
  memmapRunLayout();
}

// Apply the source filter + search highlight to the overview using cy selectors.
function memmapApplyFilters() {
  // Brain mode: only a concept-name search highlight (no source/cluster filter).
  if (memmapViewMode === 'brain') {
    if (!memmapCy || brainConcept) return;
    const needle = memmapSearch.trim().toLowerCase();
    memmapCy.nodes().removeClass('dim hit');
    if (needle) {
      memmapCy.nodes().filter(n => !String(n.data('rawLabel') || '').toLowerCase().includes(needle)).addClass('dim');
      memmapCy.nodes().filter(n => String(n.data('rawLabel') || '').toLowerCase().includes(needle)).removeClass('dim').addClass('hit');
    }
    return;
  }
  // Bubble levels only (overview / group); leaf shows members, no cluster filter.
  if (!memmapCy || (memmapMode !== 'overview' && memmapMode !== 'group')) return;
  const needle = memmapSearch.trim().toLowerCase();
  memmapCy.nodes().removeClass('dim hit');
  // Source filter: dim nodes whose dominant_source doesn't match (mixed always shows).
  if (memmapSource !== 'all') {
    memmapCy.nodes().filter(n => {
      const dom = n.data('dom') || 'mixed';
      return dom !== 'mixed' && dom !== memmapSource;
    }).addClass('dim');
  }
  // Search: highlight matching cluster labels.
  if (needle) {
    memmapCy.nodes().filter(n => !String(n.data('rawLabel') || '').toLowerCase().includes(needle)).addClass('dim');
    memmapCy.nodes().filter(n => String(n.data('rawLabel') || '').toLowerCase().includes(needle)).removeClass('dim').addClass('hit');
  }
}

// Offline / library-load failure: render clusters as a plain clickable list.
function renderMemmapFallback(clusters) {
  const fb = document.getElementById('memmap-fallback');
  if (!fb) return;
  const catchAllId = memmapCatchAllId(clusters);
  const note = '<div class="empty" style="color:var(--warn);">Graph library failed to load (offline?) — showing a list instead.</div>';
  const rows = clusters
    .filter(c => c.id !== catchAllId)
    .sort((a, b) => Number(b.size || 0) - Number(a.size || 0))
    .map(c => {
      const size = Number(c.size || 0);
      const dom = c.dominant_source || 'mixed';
      const badge = dom === 'subagent' ? 'badge source-subagent' : 'badge';
      const tok = c.token != null ? String(c.token) : '';
      const drillable = c.drillable !== false && tok;
      return `<div class="member-row" data-token="${escapeHtml(tok)}" data-drillable="${drillable ? '1' : '0'}" data-label="${escapeHtml(String(c.label || ''))}">`
        + `<span class="mr-badge ${badge}">${memmapSourceLabelKo(dom === 'subagent' ? 'subagent' : 'main')}</span>`
        + `<div class="mr-body"><div class="mr-preview">${escapeHtml(String(c.label || '(no name)'))}</div>`
        + `<div class="mr-kind">${size.toLocaleString()}</div></div></div>`;
    }).join('');
  fb.innerHTML = note + rows;
  fb.hidden = false;
  fb.querySelectorAll('.member-row').forEach(el => {
    el.onclick = () => {
      if (el.dataset.drillable !== '1') return;
      memmapDrill(el.dataset.token, el.dataset.label || el.dataset.token);
    };
  });
}

// Offline member fallback list for drill-down.
function renderMemmapMemberFallback(payload) {
  const fb = document.getElementById('memmap-fallback');
  if (!fb) return;
  const nodes = (payload.nodes || []).filter(n => n.node_type === 'memory' || n.node_type === undefined);
  if (!nodes.length) {
    fb.innerHTML = '<div class="empty">No memories in this cluster.</div>';
    fb.hidden = false;
    return;
  }
  fb.innerHTML = nodes.map(n => {
    const source = n.source_kind || 'main';
    const badgeClass = source === 'subagent' ? 'badge source-subagent' : 'badge';
    const preview = String(n.label || n.preview || n.body || n.id || '');
    const kind = n.kind ? `<div class="mr-kind">${escapeHtml(String(n.kind))}</div>` : '';
    return `<div class="member-row" data-id="${escapeHtml(String(n.id || ''))}">`
      + `<span class="mr-badge ${badgeClass}">${memmapSourceLabelKo(source)}</span>`
      + `<div class="mr-body"><div class="mr-preview">${escapeHtml(preview)}</div>${kind}</div>`
      + `</div>`;
  }).join('');
  fb.hidden = false;
  fb.querySelectorAll('.member-row').forEach(el => {
    el.onclick = () => {
      const num = String(el.dataset.id || '').replace(/^m/, '');
      if (/^\d+$/.test(num)) openDetailModal(Number(num));
    };
  });
}

// Render a cached level payload by its declared mode (group bubbles vs leaf members).
function memmapRenderLevelPayload(payload, label) {
  if (payload && payload.mode === 'leaf') memmapRenderLeafLevel(payload, label);
  else memmapRenderGroupLevel(payload, label);
}

// Drill into one bubble by token: fetch the level (cached by token), push a crumb,
// and render it (group bubbles or leaf members). On a stale (HTTP 410) token the
// whole map is reloaded from the overview.
async function memmapDrill(token, label) {
  token = String(token || '');
  label = String(label || token);
  if (!token) return;
  const head = document.getElementById('memmap-overview-head');

  let payload = memmapLevelCache.get(token);
  if (!payload) {
    if (head) head.textContent = `${label} · Loading…`;
    try {
      // depth(leaf) is re-sent on every drill so the server applies the same
      // depth at each level (the token only carries the member set).
      const dp = memmapDepth ? `&leaf=${memmapDepth}` : '';
      const r = await fetch('/api/memory/graph?token=' + encodeURIComponent(token) + dp);
      if (r.status === 410) {
        showToast('The map changed — reloading from the start', 'err');
        await loadGraph(currentProject());
        return;
      }
      if (!r.ok) {
        showToast(`Failed to load group ${r.status}`, 'err');
        if (head) head.textContent = `${label} · error ${r.status}`;
        return;
      }
      payload = await r.json();
      memmapLevelCache.set(token, payload);
    } catch (e) {
      showToast(`Network error: ${e.message || e}`, 'err');
      if (head) head.textContent = `${label} · Network error`;
      return;
    }
  }
  memmapStack.push({ token, label });
  memmapRenderLevelPayload(payload, label);
}

// Jump to a breadcrumb depth (number of pushed levels to keep). depth 0 = overview
// root; depth N renders the Nth pushed level from cache. Pops the stack to match.
function memmapGoToDepth(depth) {
  if (depth <= 0) { renderMemmapOverview(); return; }
  memmapStack = memmapStack.slice(0, depth);
  const crumb = memmapStack[memmapStack.length - 1];
  const payload = crumb ? memmapLevelCache.get(crumb.token) : null;
  if (!payload) { renderMemmapOverview(); return; }
  memmapRenderLevelPayload(payload, crumb.label);
}

// ── DIGITAL BRAIN (concept graph) ───────────────────────────────────────────────
// Obsidian-style concept map: nodes are CONCEPTS (salient tokens), edges are
// co-occurrence between them. Works per-project and GLOBAL (All = all projects
// merged into one brain). Reuses the shared cy instance, styling, layout +
// hover card. No LLM — the server derives concepts from the token index.

// Concept node radius is driven by DEGREE: r = clamp(8 + degree*2.5, 8, 22)
// (same spec already used by the cluster view). cy 'width'/'height' want the
// DIAMETER, so we return 2r.
function brainNodeDiameter(degree) {
  const r = Math.max(8, Math.min(22, 8 + Number(degree || 0) * 2.5));
  return Math.round(r * 2);
}

// Deterministic palette colour per concept (clean Memory map gold/blue/green,
// never an 18-colour rainbow). Hash the token so the same concept keeps its hue.
const BRAIN_CONCEPT_PALETTE = [
  MEMMAP_PALETTE.main, MEMMAP_PALETTE.subagent, MEMMAP_PALETTE.mixed,
];
function brainConceptColor(token) {
  const s = String(token || '');
  let h = 0;
  for (let i = 0; i < s.length; i += 1) h = (h * 31 + s.charCodeAt(i)) >>> 0;
  return BRAIN_CONCEPT_PALETTE[h % BRAIN_CONCEPT_PALETTE.length];
}

// Build the Cytoscape elements for the brain overview from { nodes, edges }.
// Nodes carry kindType:'concept', size-by-degree, token for the drill, and a
// `hub` flag (top-degree nodes keep their label visible).
function brainOverviewElements(payload) {
  const rawNodes = Array.isArray(payload.nodes) ? payload.nodes : [];
  const rawEdges = Array.isArray(payload.edges) ? payload.edges : [];
  const ids = new Set(rawNodes.map(n => String(n.id)));
  // Hubs: the top ~12 by degree get a persistent label so the map has anchors.
  const byDegree = rawNodes.slice().sort((a, b) => Number(b.degree || 0) - Number(a.degree || 0));
  const hubIds = new Set(byDegree.slice(0, 12).map(n => String(n.id)));
  const nodes = rawNodes.map(n => {
    const id = String(n.id);
    const token = String(n.label != null ? n.label : id.replace(/^c:/, ''));
    const degree = Number(n.degree || 0);
    const freq = Number(n.freq || 0);
    const projects = Array.isArray(n.projects) ? n.projects : [];
    return {
      data: {
        id, label: token, rawLabel: token, token,
        size: brainNodeDiameter(degree),
        col: brainConceptColor(token),
        kindType: 'concept',
        degree, freq, projects,
        hub: hubIds.has(id) ? 1 : undefined,
      },
    };
  });
  const edgeEls = rawEdges
    .filter(e => ids.has(String(e.src)) && ids.has(String(e.dst)) && e.src !== e.dst)
    .map((e, i) => ({
      data: {
        id: `be${i}`,
        source: String(e.src),
        target: String(e.dst),
        w: Math.max(1, Math.min(4, Number(e.weight || 1))),
      },
    }));
  return nodes.concat(edgeEls);
}

// Build the Cytoscape elements for the brain TOP LEVEL: TOPIC COMMUNITIES.
// Nodes carry kindType:'community' (hexagon super-node), sized by the community's
// SIZE (memories touched) with concept_count as a tie-break, clamped so a few big
// topics read clearly while small ones stay legible. Inter-community edges are
// thinned to a backbone (same as concepts) to keep the top view airy.
function brainCommunityElements(payload) {
  const rawNodes = Array.isArray(payload.nodes) ? payload.nodes : [];
  const rawEdges = Array.isArray(payload.edges) ? payload.edges : [];
  const ids = new Set(rawNodes.map(n => String(n.id)));
  // Sqrt scale on size keeps small communities visible; clamp to a hexagon range
  // bigger than concept ideas so topics dominate the canvas.
  const sizes = rawNodes.map(n => Number(n.size || 0));
  const maxSize = Math.max(1, ...sizes);
  const SCALE_MIN = 28, SCALE_MAX = 72;
  const nodes = rawNodes.map(n => {
    const id = String(n.id);
    const label = String(n.label != null ? n.label : id.replace(/^k:/, ''));
    const size = Number(n.size || 0);
    const conceptCount = Number(n.concept_count || 0);
    const tops = Array.isArray(n.top_concepts) ? n.top_concepts.map(String) : [];
    const diam = SCALE_MIN + (SCALE_MAX - SCALE_MIN) * Math.sqrt(size / maxSize);
    return {
      data: {
        id, label, rawLabel: label,
        size: Math.round(Math.max(SCALE_MIN, Math.min(SCALE_MAX, diam))),
        col: brainConceptColor(label),
        kindType: 'community',
        // The community id the API drills by (strip the "k:" id prefix).
        communityId: id.replace(/^k:/, ''),
        conceptCount, topConcepts: tops,
      },
    };
  });
  const edgeEls = memmapBackboneEdges(
    rawEdges.map(e => ({ src: e.src, dst: e.dst, weight: e.weight })), ids,
  ).map((e, i) => ({
    data: {
      id: `ke${i}`,
      source: String(e.src),
      target: String(e.dst),
      w: Math.max(1, Math.min(4, Number(e.w || 1))),
    },
  }));
  return nodes.concat(edgeEls);
}

// Render the breadcrumb for the brain: All(topics) › <topic label> › <concept label>.
// The communities overview is the implicit root; pushed levels follow. Clicking a
// non-current crumb pops back to that level; the current crumb is inert.
function brainRenderCrumbs() {
  const bar = document.getElementById('memmap-crumbs');
  if (!bar) return;
  bar.innerHTML = '';
  // Hidden at the top level — the communities overview shows no breadcrumb.
  if (!brainStack.length) { bar.hidden = true; return; }
  const crumbs = [{ kind: 'root', label: 'All(Topic)' }].concat(brainStack);
  crumbs.forEach((c, i) => {
    if (i > 0) {
      const sep = document.createElement('span');
      sep.className = 'crumb-sep';
      sep.textContent = '›';
      bar.appendChild(sep);
    }
    const isLast = i === crumbs.length - 1;
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'crumb' + (isLast ? ' current' : '');
    btn.textContent = c.label;
    if (!isLast) btn.onclick = () => brainGoToDepth(i);   // i = pushed levels to keep
    bar.appendChild(btn);
  });
  bar.hidden = false;
}

// Pop the brain breadcrumb to a given depth (number of pushed levels to keep).
// depth 0 = communities root; depth 1 = the community's concepts (from cache).
function brainGoToDepth(depth) {
  if (depth <= 0) { renderBrainOverview(); return; }
  // Only depth 1 is a re-rendered level (community → concepts). The concept level
  // (depth 2) is a memory list and is never a "go back" target (it's the leaf).
  const crumb = brainStack[depth - 1];
  if (!crumb || crumb.kind !== 'community') { renderBrainOverview(); return; }
  brainStack = brainStack.slice(0, depth);
  const payload = brainCommunityCache.get(String(crumb.id));
  if (!payload) { renderBrainOverview(); return; }
  renderBrainCommunityLevel(crumb.id, crumb.label, payload);
}

// #memmap-back pops exactly one brain level:
//   memory (concept drill) → back to the community's concepts
//   concept (community drill) → back to the communities overview
function brainBack() {
  if (brainLevel === 'memory' && brainCommunity) {
    const cached = brainCommunityCache.get(String(brainCommunity.id));
    if (cached) {
      brainStack = [{ kind: 'community', id: brainCommunity.id, label: brainCommunity.label }];
      renderBrainCommunityLevel(brainCommunity.id, brainCommunity.label, cached);
      return;
    }
  }
  renderBrainOverview();
}

// Render the brain TOP LEVEL = TOPIC COMMUNITIES (or fallback list if cy is
// unavailable). Resets the drill state so we're back at the topic constellation.
function renderBrainOverview() {
  brainConcept = null;
  brainCommunity = null;
  brainLevel = 'community';
  brainStack = [];
  const head = document.getElementById('memmap-overview-head');
  memmapResetViews();
  memmapStack = [];
  const back = document.getElementById('memmap-back');
  if (back) back.hidden = true;   // top level: no Back
  brainRenderCrumbs();            // hidden at the top
  if (!brainGraph) {
    if (head) head.textContent = '';
    if (memmapCy) memmapCy.elements().remove();
    return;
  }
  const communities = (Array.isArray(brainGraph.nodes) ? brainGraph.nodes : []);
  const concepts = Number(brainGraph.total_concepts || 0);
  const mems = Number(brainGraph.total_memories || 0);
  const scopeLabel = brainScopeGlobal ? 'All' : (brainProject || '');
  if (head) {
    head.textContent = `Topics ${communities.length.toLocaleString()} · `
      + `Concepts ${concepts.toLocaleString()} · Memories ${mems.toLocaleString()} · ${scopeLabel}`;
  }
  if (!communities.length) {
    const empty = document.getElementById('memmap-empty');
    if (empty) { empty.textContent = 'Not enough topics yet (they appear as more memories accumulate).'; empty.hidden = false; }
    if (memmapCy) memmapCy.elements().remove();
    return;
  }
  const cy = memmapEnsureCy();
  if (!cy) { renderBrainCommunityFallback(brainGraph); return; }
  cy.elements().remove();
  cy.add(brainCommunityElements(brainGraph));
  memmapApplyFilters();
  memmapRunLayout();
}

// Drill a TOPIC COMMUNITY → its CONCEPTS. Fetches mode=brain&community=ID for the
// current scope, pushes a breadcrumb, and renders the member concepts (existing
// concept node style). Tapping a concept then drills to its memories.
async function brainDrillCommunity(communityId, label) {
  communityId = String(communityId || '');
  label = String(label || communityId);
  if (communityId === '') return;
  const head = document.getElementById('memmap-overview-head');
  let payload = brainCommunityCache.get(communityId);
  if (!payload) {
    if (head) head.textContent = `Topic "${label}" · Loading…`;
    try {
      const params = new URLSearchParams({ mode: 'brain', community: communityId });
      if (!brainScopeGlobal && brainProject) params.set('project', brainProject);
      const r = await fetch(`/api/memory/graph?${params.toString()}`);
      if (!r.ok) {
        showToast(`Failed to load topic ${r.status}`, 'err');
        if (head) head.textContent = `Topic "${label}" · error ${r.status}`;
        return;
      }
      payload = await r.json();
      payload.nodes = Array.isArray(payload.nodes) ? payload.nodes : [];
      payload.edges = Array.isArray(payload.edges) ? payload.edges : [];
      brainCommunityCache.set(communityId, payload);
    } catch (e) {
      showToast(`Network error: ${e.message || e}`, 'err');
      if (head) head.textContent = `Topic "${label}" · Network error`;
      return;
    }
  }
  brainStack = [{ kind: 'community', id: communityId, label }];
  renderBrainCommunityLevel(communityId, label, payload);
}

// Render one community's CONCEPTS (the second brain level). Reuses the concept
// node style + layout. Assumes the crumb is already on brainStack.
function renderBrainCommunityLevel(communityId, label, payload) {
  brainConcept = null;
  brainCommunity = { id: String(communityId), label: String(label) };
  brainLevel = 'concept';
  const head = document.getElementById('memmap-overview-head');
  memmapResetViews();
  const back = document.getElementById('memmap-back');
  if (back) back.hidden = false;
  brainRenderCrumbs();
  const concepts = (Array.isArray(payload.nodes) ? payload.nodes : []);
  if (head) head.textContent = `Topic "${label}" · Concepts ${concepts.length.toLocaleString()}`;
  if (!concepts.length) {
    const empty = document.getElementById('memmap-empty');
    if (empty) { empty.textContent = 'No concepts in this topic.'; empty.hidden = false; }
    if (memmapCy) memmapCy.elements().remove();
    return;
  }
  const cy = memmapEnsureCy();
  if (!cy) { renderBrainFallback(payload); return; }
  cy.elements().remove();
  cy.add(brainOverviewElements(payload));
  memmapApplyFilters();
  memmapRunLayout();
}

// Offline / no-cy fallback for the TOP LEVEL: list communities as clickable rows.
function renderBrainCommunityFallback(payload) {
  const fb = document.getElementById('memmap-fallback');
  if (!fb) return;
  const nodes = (Array.isArray(payload.nodes) ? payload.nodes : [])
    .slice().sort((a, b) => Number(b.size || 0) - Number(a.size || 0));
  const note = '<div class="empty" style="color:var(--warn);">Graph library failed to load (offline?) — showing a list instead.</div>';
  fb.innerHTML = note + nodes.map(n => {
    const id = String(n.id != null ? n.id : '').replace(/^k:/, '');
    const label = String(n.label != null ? n.label : id);
    const size = Number(n.size || 0);
    const cnt = Number(n.concept_count || 0);
    const tops = Array.isArray(n.top_concepts) ? n.top_concepts.map(String) : [];
    const sub = tops.length ? ` · ${escapeHtml(tops.join(', '))}` : '';
    return `<div class="member-row" data-community="${escapeAttr(id)}" data-label="${escapeAttr(label)}">`
      + `<span class="mr-badge badge">Topic</span>`
      + `<div class="mr-body"><div class="mr-preview">${escapeHtml(label)}</div>`
      + `<div class="mr-kind">${cnt.toLocaleString()} concepts · Memory ${size.toLocaleString()}${sub}</div></div></div>`;
  }).join('');
  fb.hidden = false;
  fb.querySelectorAll('.member-row').forEach(el => {
    el.onclick = () => brainDrillCommunity(el.dataset.community, el.dataset.label || el.dataset.community);
  });
}

// Offline / no-cy fallback for a community's CONCEPTS: clickable concept rows.
function renderBrainFallback(payload) {
  const fb = document.getElementById('memmap-fallback');
  if (!fb) return;
  const nodes = (Array.isArray(payload.nodes) ? payload.nodes : [])
    .slice().sort((a, b) => Number(b.degree || 0) - Number(a.degree || 0));
  const note = '<div class="empty" style="color:var(--warn);">Graph library failed to load (offline?) — showing a list instead.</div>';
  fb.innerHTML = note + nodes.map(n => {
    const token = String(n.label != null ? n.label : String(n.id).replace(/^c:/, ''));
    const degree = Number(n.degree || 0);
    const freq = Number(n.freq || 0);
    return `<div class="member-row" data-concept="${escapeAttr(token)}">`
      + `<span class="mr-badge badge">Concept</span>`
      + `<div class="mr-body"><div class="mr-preview">${escapeHtml(token)}</div>`
      + `<div class="mr-kind">${degree} links · ${freq.toLocaleString()}</div></div></div>`;
  }).join('');
  fb.hidden = false;
  fb.querySelectorAll('.member-row').forEach(el => {
    el.onclick = () => brainDrillConcept(el.dataset.concept);
  });
}

// Drill a concept → its MEMORIES (the third/leaf brain level). Fetches
// mode=brain&concept=TOKEN for the current scope, pushes a breadcrumb, and renders
// the memories AS member nodes; tapping one opens the memory modal. Back / the
// crumb returns to the community's concepts.
async function brainDrillConcept(concept) {
  concept = String(concept || '');
  if (!concept) return;
  brainConcept = concept;
  brainLevel = 'memory';
  // Push the concept crumb (drop any stale concept crumb from a previous drill so
  // we never stack All(topics) › topic › conceptA › conceptB).
  if (brainStack.length && brainStack[brainStack.length - 1].kind === 'concept') {
    brainStack = brainStack.slice(0, -1);
  }
  brainStack.push({ kind: 'concept', token: concept, label: concept });
  brainRenderCrumbs();
  const head = document.getElementById('memmap-overview-head');
  if (head) head.textContent = `Concept "${concept}" · Loading…`;
  const back = document.getElementById('memmap-back');
  if (back) back.hidden = false;
  // Clear the graph canvas while the memories load (we show a list, not a graph).
  if (memmapCy) memmapCy.elements().remove();
  memmapStopLayout();
  try {
    const params = new URLSearchParams({ mode: 'brain', concept });
    if (!brainScopeGlobal && brainProject) params.set('project', brainProject);
    const r = await fetch(`/api/memory/graph?${params.toString()}`);
    if (!r.ok) {
      showToast(`Failed to load concept ${r.status}`, 'err');
      if (head) head.textContent = `Concept "${concept}" · error ${r.status}`;
      return;
    }
    const d = await r.json();
    const mems = (Array.isArray(d.nodes) ? d.nodes : [])
      .filter(n => n.node_type === 'memory' || n.node_type === undefined);
    if (head) head.textContent = `Concept "${concept}" · Memories ${mems.length.toLocaleString()}`;
    // Render the concept's memories AS NODES inside the same graph (not a hidden
    // list below the canvas — that left the canvas blank). Member tap -> modal.
    const cy = memmapEnsureCy();
    if (!cy) { renderBrainConceptMemories(concept, d); return; }
    memmapResetViews();
    cy.elements().remove();
    cy.add(memmapMemberElements(d));
    memmapRunLayout();
  } catch (e) {
    showToast(`Network error: ${e.message || e}`, 'err');
    if (head) head.textContent = `Concept "${concept}" · Network error`;
  }
}

// Render a concept's memories as a clickable list: preview + project badge +
// source. Clicking a row opens the memory modal.
function renderBrainConceptMemories(concept, payload) {
  const head = document.getElementById('memmap-overview-head');
  const fb = document.getElementById('memmap-fallback');
  const empty = document.getElementById('memmap-empty');
  const nodes = (payload && Array.isArray(payload.nodes) ? payload.nodes : [])
    .filter(n => n.node_type === 'memory' || n.node_type === undefined);
  if (head) head.textContent = `Concept "${concept}" · Memories ${nodes.length.toLocaleString()}`;
  if (!fb) return;
  if (!nodes.length) {
    if (empty) { empty.textContent = 'No memories for this concept.'; empty.hidden = false; }
    fb.hidden = true; fb.innerHTML = '';
    return;
  }
  if (empty) empty.hidden = true;
  fb.innerHTML = nodes.map(n => {
    const source = n.source_kind || 'main';
    const badgeClass = source === 'subagent' ? 'badge source-subagent' : 'badge';
    const preview = String(n.label || n.preview || n.body || n.id || '');
    const proj = n.project ? `<span class="mr-badge badge">${escapeHtml(String(n.project))}</span>` : '';
    const kind = n.kind ? `<div class="mr-kind">${escapeHtml(String(n.kind))}</div>` : '';
    return `<div class="member-row" data-id="${escapeAttr(String(n.id || ''))}">`
      + `<span class="mr-badge ${badgeClass}">${memmapSourceLabelKo(source)}</span>${proj}`
      + `<div class="mr-body"><div class="mr-preview">${escapeHtml(preview)}</div>${kind}</div>`
      + `</div>`;
  }).join('');
  fb.hidden = false;
  fb.querySelectorAll('.member-row').forEach(el => {
    el.onclick = () => {
      const num = String(el.dataset.id || '').replace(/^m/, '');
      if (/^\d+$/.test(num)) openDetailModal(Number(num));
    };
  });
}

// Load the brain for a project, OR the GLOBAL brain when global is set. Unlike
// the cluster loader, this NEVER shows the "Select a project" message: global
// scope loads the merged brain (All).
async function loadBrain(project) {
  brainConcept = null;
  brainCommunity = null;
  brainLevel = 'community';
  brainStack = [];
  brainCommunityCache.clear();   // scope/project changed — drop the old sub-graphs
  memmapStopLayout();
  memmapResetViews();
  const back = document.getElementById('memmap-back');
  if (back) back.hidden = true;
  const crumbs = document.getElementById('memmap-crumbs');
  if (crumbs) { crumbs.hidden = true; crumbs.innerHTML = ''; }
  const head = document.getElementById('memmap-overview-head');
  if (memmapCy) memmapCy.elements().remove();
  const global = isGlobalScope() || isGlobalProjectValue(project) || !project;
  brainScopeGlobal = global;
  brainProject = global ? '' : project;
  if (head) head.textContent = 'Loading…';
  try {
    const params = new URLSearchParams({ mode: 'brain' });
    // Omit project for the GLOBAL brain (server treats empty/sentinel as global).
    if (!global) params.set('project', project);
    const r = await fetch(`/api/memory/graph?${params.toString()}`);
    if (!r.ok) {
      const text = await r.text();
      showToast(`Failed to load brain ${r.status}: ${text}`, 'err');
      if (head) head.textContent = `Error ${r.status}`;
      brainGraph = null;
      return;
    }
    const d = await r.json();
    d.nodes = Array.isArray(d.nodes) ? d.nodes : [];
    d.edges = Array.isArray(d.edges) ? d.edges : [];
    brainGraph = d;
    renderBrainOverview();
  } catch (e) {
    showToast(`Network error: ${e.message || e}`, 'err');
    if (head) head.textContent = 'Network error';
    brainGraph = null;
  }
}

// The map is the digital brain (concept graph). Always loads the brain — works
// per-project AND global (never the "select a project" message).
function loadMemmap(project) {
  loadBrain(project);
}

// Switch the map between brain(concept) and bundle(cluster). Hides the cluster-only
// controls (basis/granularity/depth/source/embeddings) in brain mode and runs the matching loader.
function memmapSetViewMode(mode, project) {
  memmapViewMode = mode === 'cluster' ? 'cluster' : 'brain';
  document.querySelectorAll('#memmap-mode-toggle .sort-btn').forEach(b => {
    b.classList.toggle('active', b.dataset.memmapMode === memmapViewMode);
  });
  const isBrain = memmapViewMode === 'brain';
  const clusterCtl = document.getElementById('memmap-cluster-controls');
  if (clusterCtl) clusterCtl.style.display = isBrain ? 'none' : 'contents';
  const srcFilter = document.getElementById('memmap-source-filter');
  if (srcFilter) srcFilter.style.display = isBrain ? 'none' : '';
  const embedBtn = document.getElementById('memmap-embed-btn');
  if (embedBtn) embedBtn.style.display = isBrain ? 'none' : '';
  const search = document.getElementById('memmap-search');
  if (search) search.placeholder = isBrain ? 'Search concepts' : 'Search clusters';
  loadMemmap(project || memmapProject || brainProject || currentProject());
}

// Load the overview graph for a project with the current grouping basis. Always
// lands on the overview (resets the breadcrumb stack + per-level token cache).
async function loadGraph(project) {
  memmapStack = [];
  memmapLevelCache.clear();
  memmapSetMode('overview');
  memmapRenderCrumbs();
  memmapResetViews();
  const head = document.getElementById('memmap-overview-head');
  if (memmapCy) memmapCy.elements().remove();
  if (isGlobalScope() || isGlobalProjectValue(project)) {
    if (head) head.textContent = GLOBAL_SCOPE_MESSAGE;
    memmapOverview = null;
    return;
  }
  if (head) head.textContent = 'Loading…';
  try {
    const params = new URLSearchParams({ project, mode: 'overview', group: memmapGroup });
    // Only send the semantic/lexical + granularity knobs for the context basis (meaningless for meta facets).
    if (memmapGroup === 'context') params.set('basis', memmapBasis);
    params.set('target', String(memmapTarget));
    const r = await fetch(`/api/memory/graph?${params.toString()}`);
    if (!r.ok) {
      const text = await r.text();
      showToast(`Failed to load knowledge map ${r.status}: ${text}`, 'err');
      if (head) head.textContent = `Error ${r.status}`;
      memmapOverview = null;
      return;
    }
    const d = await r.json();
    d.clusters = Array.isArray(d.clusters) ? d.clusters : [];
    d.cluster_edges = Array.isArray(d.cluster_edges) ? d.cluster_edges : [];
    d.total_nodes = Number(d.total_nodes || 0);
    if (d.group) memmapGroup = String(d.group);   // echo the server's resolved basis
    memmapOverview = d;
    memmapProject = project;
    renderMemmapOverview();
  } catch (e) {
    showToast(`Network error: ${e.message || e}`, 'err');
    if (head) head.textContent = 'Network error';
    memmapOverview = null;
  }
}

// Wire up the controls: search box, group basis, source filter, back, fit.
(function wireMemmap() {
  // Map mode toggle: brain(concept) ↔ bundle(cluster). Default brain (active in markup).
  document.querySelectorAll('#memmap-mode-toggle .sort-btn').forEach(btn => {
    btn.onclick = () => memmapSetViewMode(btn.dataset.memmapMode || 'brain');
  });
  const search = document.getElementById('memmap-search');
  if (search) search.oninput = () => { memmapSearch = search.value || ''; memmapApplyFilters(); };
  // Grouping basis: changing it resets the breadcrumb + cache and reloads overview.
  const group = document.getElementById('memmap-group');
  if (group) group.onchange = () => {
    memmapGroup = group.value || 'context';
    const project = memmapProject || currentProject();
    if (project) loadGraph(project);
  };
  // classification basis (auto/semantic/lexical) · granularity (bubble count) · depth (drill) — view-level knobs,
  // each reloads the overview with the new value.
  const reloadMap = () => {
    const project = memmapProject || currentProject();
    if (project && !isGlobalScope() && !isGlobalProjectValue(project)) loadGraph(project);
  };
  const basisSel = document.getElementById('memmap-basis');
  if (basisSel) basisSel.onchange = () => { memmapBasis = basisSel.value || 'auto'; reloadMap(); };
  const targetSel = document.getElementById('memmap-target');
  if (targetSel) targetSel.onchange = () => { memmapTarget = Number(targetSel.value) || 320; reloadMap(); };
  const depthSel = document.getElementById('memmap-depth');
  if (depthSel) depthSel.onchange = () => { memmapDepth = Number(depthSel.value) || 0; reloadMap(); };
  document.querySelectorAll('#memmap-source-filter .sort-btn').forEach(btn => {
    btn.onclick = () => {
      memmapSource = btn.dataset.memmapSource || 'all';
      document.querySelectorAll('#memmap-source-filter .sort-btn').forEach(b => {
        b.classList.toggle('active', b.dataset.memmapSource === memmapSource);
      });
      memmapApplyFilters();
    };
  });
  // Back: in brain mode, return from a concept's memories to the concept brain;
  // in cluster mode, pop exactly one breadcrumb level (root reloads the overview).
  const back = document.getElementById('memmap-back');
  if (back) back.onclick = () => {
    if (memmapViewMode === 'brain') { brainBack(); return; }
    memmapGoToDepth(memmapStack.length - 1);
  };
  const fit = document.getElementById('memmap-fit');
  if (fit) fit.onclick = () => { if (memmapCy) memmapCy.fit(undefined, 30); };
  // Embed this project (background) right from the map; caption shows progress.
  const embedBtn = document.getElementById('memmap-embed-btn');
  if (embedBtn) embedBtn.onclick = async () => {
    const project = memmapProject || currentProject();
    if (!project || isGlobalScope() || isGlobalProjectValue(project)) {
      showToast('Select a project first.', 'err');
      return;
    }
    const head = document.getElementById('memmap-overview-head');
    await startEmbedAndPoll(project, (t) => { if (head) head.textContent = t; });
  };
})();

document.getElementById('graph-form').onsubmit = async (ev) => {
  ev.preventDefault();
  // The map is the brain — loads per-project AND global, no scope guard.
  await loadMemmap(currentProject());
};

// Show/hide LLM model dropdown when engine switches.
document.getElementById('compress-mode').addEventListener('change', function () {
  const llmRow = document.getElementById('compress-llm-row');
  const isLlm = this.value === 'llm';
  llmRow.style.display = isLlm ? '' : 'none';
  if (isLlm && !document.getElementById('compress-llm-model').options.length) {
    // Populate from cache in case it wasn't there at page-load time.
    document.getElementById('compress-llm-model').innerHTML = buildModelOptions('');
  }
});

function renderOptimizerLevel(level, active) {
  const normalized = normalizedOptimizerLevel(level);
  const enabled = isOptimizerActive(normalized, active);
  // In "Follow global" mode for a project the level is read-only (inherited),
  // so keep the buttons disabled; otherwise they are editable.
  const editable = !optimizerLevelLocked();
  document.querySelectorAll('#optimizer-level-buttons button').forEach(btn => {
    btn.classList.toggle('active', btn.dataset.level === normalized);
    btn.disabled = !editable;
  });
  const status = document.getElementById('optimizer-level-status');
  if (status) {
    status.classList.toggle('ok', enabled);
    status.classList.toggle('inactive', !enabled);
    status.textContent = enabled ? `Terse mode: ${normalized.toUpperCase()} (active)` : 'OFF';
  }
}

// Build the `?project=` query for the optimizer level GET/POST so the
// per-project override in <repo>/.rtrt/config.toml is read/written. Global
// scope (or no project) sends no project, so the global level is used (inherit).
function optimizerProjectQuery() {
  const project = currentProject();
  if (!project || isGlobalProjectValue(project)) return '';
  return `?project=${encodeURIComponent(project)}`;
}

// True when a specific (non-global) project is selected. The Scope toggle and
// the "Follow global / Custom" behaviour only apply in that case.
function optimizerHasProject() {
  const project = currentProject();
  return !!project && !isGlobalProjectValue(project);
}

// True when the level buttons should be locked read-only: a project selected
// and currently following global (the inherited value is shown, not editable).
function optimizerLevelLocked() {
  return optimizerHasProject() && document.getElementById('optimizer-scope-global').checked;
}

// Reflect the resolved scope in the UI: show/hide the Scope section, set the
// radios, and update the source hint. The button disabled-state is applied by
// renderOptimizerLevel (which reads optimizerLevelLocked).
function applyOptimizerScope(scope) {
  const section = document.getElementById('optimizer-scope-section');
  const hint = document.getElementById('optimizer-scope-hint');
  const cfgHint = document.getElementById('optimizer-level-config-hint');
  if (!optimizerHasProject()) {
    // Global scope (or no project): no per-project toggle — edit global default.
    section.hidden = true;
    if (cfgHint) cfgHint.textContent = '~/.rtrt/output-style';
    return;
  }
  section.hidden = false;
  const custom = scope === 'custom';
  document.getElementById('optimizer-scope-global').checked = !custom;
  document.getElementById('optimizer-scope-custom').checked = custom;
  if (cfgHint) cfgHint.textContent = custom ? '<repo>/.rtrt/config.toml' : '~/.rtrt/output-style (inherited)';
  if (hint) {
    hint.textContent = custom
      ? 'Custom: pick a level below to write this project’s override.'
      : 'Follow global: this project inherits the global terse level. The level below shows the inherited value (read-only).';
  }
}

function renderOptimizerRuleSummary(source) {
  const el = document.getElementById('optimizer-rule-summary');
  if (!el) return;
  el.textContent = optimizerRuleSummary(source || {});
}

async function loadOptimizerRuleSummary() {
  const params = new URLSearchParams();
  if (currentProject() && !isGlobalScope()) params.set('project', currentProject());
  params.set('window', overviewWindow);
  const url = `/api/overview${params.toString() ? '?' + params.toString() : ''}`;
  try {
    const r = await fetch(url);
    if (!r.ok) return;
    const d = await r.json();
    const source = Array.isArray(d.sources)
      ? d.sources.find(s => s.name === 'output_optimizer')
      : null;
    renderOptimizerRuleSummary(source);
  } catch (_) { /* keep current UI state */ }
}

async function loadOptimizerLevel() {
  try {
    const r = await fetch(`/api/optimizer/level${optimizerProjectQuery()}`);
    if (r.ok) {
      const d = await r.json();
      // `scope` is "custom" when this project owns an override, else "global".
      applyOptimizerScope(d.scope === 'custom' ? 'custom' : 'global');
      renderOptimizerLevel(d.level || 'off', !!d.active);
    }
  } catch (_) {
    applyOptimizerScope('global');
  }
  await loadOptimizerRuleSummary();
}

async function setOptimizerLevel(level) {
  document.querySelectorAll('#optimizer-level-buttons button').forEach(btn => { btn.disabled = true; });
  const status = document.getElementById('optimizer-level-status');
  if (status) status.textContent = 'Saving…';
  try {
    const r = await fetch(`/api/optimizer/level${optimizerProjectQuery()}`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ level }),
    });
    if (!r.ok) {
      const msg = await r.text();
      showToast(`Level Save failed ${r.status}: ${msg}`, 'err');
      await loadOptimizerLevel();
      return;
    }
    const d = await r.json();
    applyOptimizerScope(d.scope === 'custom' ? 'custom' : 'global');
    renderOptimizerLevel(d.level || level, !!d.active);
    await loadOptimizerRuleSummary();
    pushActivity(`terse mode · ${d.level || level}`);
  } catch (e) {
    showToast(`Level Save error: ${e.message || e}`, 'err');
    await loadOptimizerLevel();
  }
}

document.querySelectorAll('#optimizer-level-buttons button').forEach(btn => {
  btn.onclick = () => {
    // While Following global for a project the level is read-only.
    if (optimizerLevelLocked()) return;
    setOptimizerLevel(btn.dataset.level || 'off');
  };
});

// Scope radio handlers. Follow global clears the project override (server-side)
// and reloads (buttons become disabled, showing the inherited global level).
// Custom enables the buttons so the next click writes the project's override.
document.getElementById('optimizer-scope-global').addEventListener('change', async (ev) => {
  if (!ev.target.checked || !optimizerHasProject()) return;
  try {
    const sep = optimizerProjectQuery() ? '&' : '?';
    const r = await fetch(`/api/optimizer/level${optimizerProjectQuery()}${sep}scope=global`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ level: 'off' }),
    });
    const d = await r.json();
    if (!r.ok) throw new Error(d.error || `${r.status}`);
    pushActivity('Optimizer level now follows global');
    showToast('Following global terse level', 'ok');
    await loadOptimizerLevel();
  } catch (err) {
    showToast(`Scope error: ${err.message || err}`, 'err');
    // Revert the radio on failure.
    applyOptimizerScope('custom');
    renderOptimizerLevel(normalizedOptimizerLevel('off'), false);
  }
});

document.getElementById('optimizer-scope-custom').addEventListener('change', (ev) => {
  if (!ev.target.checked || !optimizerHasProject()) return;
  // Enable the buttons (keep the inherited level shown); nothing is persisted
  // until the user picks a level, which writes this project's override.
  applyOptimizerScope('custom');
  document.querySelectorAll('#optimizer-level-buttons button').forEach(btn => { btn.disabled = false; });
});

// ===========================================================================
// Shared per-project scope helpers (used by compression / providers / agents).
// These mirror the statusline / optimizer-level "Follow global / Custom" UX so
// all five settings behave identically. A specific (non-global) project must be
// selected for the Scope toggle to appear; otherwise edits target the global
// config (no inheritance).
// ===========================================================================

// True when a specific (non-global) project is selected.
function scopeHasProject() {
  const project = currentProject();
  return !!project && !isGlobalProjectValue(project);
}

// `?project=<name>` query for a per-project read/write, or '' for global.
function scopeProjectQuery() {
  const project = currentProject();
  if (!project || isGlobalProjectValue(project)) return '';
  return `?project=${encodeURIComponent(project)}`;
}

// Append `scope=global` to a per-project (or global) URL, choosing the right
// separator. Used by the "Follow global" clear path.
function scopeClearUrl(base) {
  const q = scopeProjectQuery();
  const sep = q ? '&' : '?';
  return `${base}${q}${sep}scope=global`;
}

// Reflect the resolved scope for a settings card: show/hide the Scope section,
// set the radios, update the source hint, and dim the fields when following
// global (read-only inherited values).
function applyScopeToggle(key, scope, opts) {
  const section = document.getElementById(`${key}-scope-section`);
  const hint = document.getElementById(`${key}-scope-hint`);
  const cfgHint = document.getElementById(`${key}-config-hint`);
  const fields = document.getElementById(`${key}-fields`);
  if (!scopeHasProject()) {
    if (section) section.hidden = true;
    if (cfgHint) cfgHint.textContent = '~/.rtrt/config.toml';
    if (fields) fields.classList.remove('statusline-fields-disabled');
    if (opts && opts.onLock) opts.onLock(false);
    return;
  }
  if (section) section.hidden = false;
  const custom = scope === 'custom';
  const globalRadio = document.getElementById(`${key}-scope-global`);
  const customRadio = document.getElementById(`${key}-scope-custom`);
  if (globalRadio) globalRadio.checked = !custom;
  if (customRadio) customRadio.checked = custom;
  if (cfgHint) cfgHint.textContent = custom ? '<repo>/.rtrt/config.toml' : '~/.rtrt/config.toml (inherited)';
  if (hint && opts && opts.hints) hint.textContent = custom ? opts.hints.custom : opts.hints.global;
  if (fields) fields.classList.toggle('statusline-fields-disabled', !custom);
  // Lock = following global → fields read-only.
  if (opts && opts.onLock) opts.onLock(!custom);
}

// ===========================================================================
// Compression level (rtrt compress default): off | lite | full | ultra |
// extreme. Per-project via ProjectConfig.compression; mirrors the optimizer
// level button UX exactly.
// ===========================================================================

function compressionLevelLocked() {
  return scopeHasProject() && document.getElementById('compression-scope-global').checked;
}

function applyCompressionScope(scope) {
  applyScopeToggle('compression', scope, {
    hints: {
      custom: 'Custom: pick a level below to write this project’s override.',
      global: 'Follow global: this project inherits the global compression level. The level below shows the inherited value (read-only).',
    },
    onLock: (locked) => {
      document.querySelectorAll('#compression-level-buttons button').forEach(btn => { btn.disabled = locked; });
    },
  });
}

function renderCompressionLevel(level) {
  const normalized = (level || 'off').toLowerCase();
  const enabled = normalized !== 'off';
  const editable = !compressionLevelLocked();
  document.querySelectorAll('#compression-level-buttons button').forEach(btn => {
    btn.classList.toggle('active', btn.dataset.level === normalized);
    btn.disabled = !editable;
  });
  const status = document.getElementById('compression-level-status');
  if (status) {
    status.classList.toggle('ok', enabled);
    status.classList.toggle('inactive', !enabled);
    status.textContent = enabled ? `Compression: ${normalized.toUpperCase()}` : 'Compression: off';
  }
}

async function loadCompressionLevel() {
  try {
    const r = await fetch(`/api/compression/config${scopeProjectQuery()}`);
    if (r.ok) {
      const d = await r.json();
      applyCompressionScope(d.scope === 'custom' ? 'custom' : 'global');
      renderCompressionLevel(d.level || 'off');
      return;
    }
  } catch (_) { /* fall through */ }
  applyCompressionScope('global');
  renderCompressionLevel('off');
}

async function setCompressionLevel(level) {
  document.querySelectorAll('#compression-level-buttons button').forEach(btn => { btn.disabled = true; });
  const status = document.getElementById('compression-level-status');
  if (status) status.textContent = 'Saving…';
  try {
    const r = await fetch(`/api/compression/config${scopeProjectQuery()}`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ level }),
    });
    const d = await r.json();
    if (!r.ok) throw new Error(d.error || `${r.status}`);
    applyCompressionScope(d.scope === 'custom' ? 'custom' : 'global');
    renderCompressionLevel(d.level || level);
    pushActivity(`compression · ${d.level || level}`);
  } catch (e) {
    showToast(`Compression save error: ${e.message || e}`, 'err');
    await loadCompressionLevel();
  }
}

document.querySelectorAll('#compression-level-buttons button').forEach(btn => {
  btn.onclick = () => {
    if (compressionLevelLocked()) return;
    setCompressionLevel(btn.dataset.level || 'off');
  };
});

document.getElementById('compression-scope-global').addEventListener('change', async (ev) => {
  if (!ev.target.checked || !scopeHasProject()) return;
  try {
    const r = await fetch(scopeClearUrl('/api/compression/config'), {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ level: 'off' }),
    });
    const d = await r.json();
    if (!r.ok) throw new Error(d.error || `${r.status}`);
    pushActivity('Compression level now follows global');
    showToast('Following global compression level', 'ok');
    await loadCompressionLevel();
  } catch (err) {
    showToast(`Scope error: ${err.message || err}`, 'err');
    applyCompressionScope('custom');
    renderCompressionLevel('off');
  }
});

document.getElementById('compression-scope-custom').addEventListener('change', (ev) => {
  if (!ev.target.checked || !scopeHasProject()) return;
  applyCompressionScope('custom');
  document.querySelectorAll('#compression-level-buttons button').forEach(btn => { btn.disabled = false; });
});

// ===========================================================================
// Providers (active provider + enabled overlay). Per-project via
// ProjectConfig.providers. Form-based: an active <select> + an enable list +
// a Save button (mirrors the statusline form's save-then-reload flow).
// ===========================================================================

let PROVIDERS_STATE = { active: null, providers: [] };

function applyProvidersScope(scope) {
  applyScopeToggle('providers', scope, {
    hints: {
      custom: 'Custom: edit the fields below, then Save to write this project’s override.',
      global: 'Follow global: this project inherits the global provider settings. The fields below show the inherited values (read-only).',
    },
    onLock: (locked) => setProvidersFieldsDisabled(locked),
  });
}

function setProvidersFieldsDisabled(disabled) {
  const active = document.getElementById('providers-active');
  if (active) active.disabled = disabled;
  document.querySelectorAll('#providers-tbl input[type="checkbox"]').forEach(cb => { cb.disabled = disabled; });
  const save = document.getElementById('providers-save-btn');
  if (save) save.disabled = disabled;
}

function renderProviders(state) {
  PROVIDERS_STATE = { active: state.active || null, providers: state.providers || [] };
  const select = document.getElementById('providers-active');
  if (select) {
    const opts = ['<option value="">(none / auto)</option>']
      .concat(PROVIDERS_STATE.providers.map(p =>
        `<option value="${escapeHtml(p.name)}"${p.name === PROVIDERS_STATE.active ? ' selected' : ''}>${escapeHtml(p.name)}</option>`));
    select.innerHTML = opts.join('');
  }
  const tbody = document.querySelector('#providers-tbl tbody');
  if (tbody) {
    if (!PROVIDERS_STATE.providers.length) {
      tbody.innerHTML = '<tr><td colspan="4" class="empty">No provider APIs detected.</td></tr>';
    } else {
      tbody.innerHTML = PROVIDERS_STATE.providers.map(p => `
        <tr>
          <td>${escapeHtml(p.name)}</td>
          <td>${p.installed ? 'yes' : 'no'}</td>
          <td>${p.has_api_key ? 'set' : '—'}</td>
          <td><input type="checkbox" class="providers-enable" data-name="${escapeHtml(p.name)}"${p.enabled ? ' checked' : ''}></td>
        </tr>`).join('');
    }
  }
}

async function loadProvidersConfig() {
  const result = document.getElementById('providers-save-result');
  if (result) result.textContent = '';
  try {
    const r = await fetch(`/api/providers/config${scopeProjectQuery()}`);
    if (r.ok) {
      const d = await r.json();
      renderProviders(d);
      applyProvidersScope(d.scope === 'custom' ? 'custom' : 'global');
      return;
    }
  } catch (_) { /* fall through */ }
  renderProviders({ active: null, providers: [] });
  applyProvidersScope('global');
}

function readProvidersForm() {
  const enabled = {};
  document.querySelectorAll('#providers-tbl .providers-enable').forEach(cb => {
    enabled[cb.dataset.name] = cb.checked;
  });
  const active = document.getElementById('providers-active').value || null;
  return { active, enabled };
}

document.getElementById('providers-save-btn').addEventListener('click', async () => {
  const result = document.getElementById('providers-save-result');
  if (result) result.textContent = 'Saving…';
  try {
    const r = await fetch(`/api/providers/config${scopeProjectQuery()}`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(readProvidersForm()),
    });
    const d = await r.json();
    if (!r.ok) throw new Error(d.error || `${r.status}`);
    renderProviders(d);
    applyProvidersScope(d.scope === 'custom' ? 'custom' : 'global');
    if (result) result.textContent = '';
    showToast('Providers saved', 'ok');
    pushActivity('Providers config saved');
  } catch (err) {
    if (result) result.innerHTML = `<span style="color:var(--err);">${escapeHtml(err.message || String(err))}</span>`;
  }
});

document.getElementById('providers-scope-global').addEventListener('change', async (ev) => {
  if (!ev.target.checked || !scopeHasProject()) return;
  const result = document.getElementById('providers-save-result');
  if (result) result.textContent = 'Switching to global…';
  try {
    const r = await fetch(scopeClearUrl('/api/providers/config'), {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(readProvidersForm()),
    });
    const d = await r.json();
    if (!r.ok) throw new Error(d.error || `${r.status}`);
    pushActivity('Providers now follow global');
    showToast('Following global providers', 'ok');
    await loadProvidersConfig();
  } catch (err) {
    if (result) result.innerHTML = `<span style="color:var(--err);">${escapeHtml(err.message || String(err))}</span>`;
    applyProvidersScope('custom');
  }
});

document.getElementById('providers-scope-custom').addEventListener('change', (ev) => {
  if (!ev.target.checked || !scopeHasProject()) return;
  applyProvidersScope('custom');
  const result = document.getElementById('providers-save-result');
  if (result) result.textContent = 'Editing project override — click Save to apply.';
});

// ===========================================================================
// Agents enable/disable (coding agents / runtimes / MCP servers). Per-project
// via ProjectConfig.agents. Form-based enable list + Save (same flow).
// ===========================================================================

let AGENTS_STATE = [];

function applyAgentsScope(scope) {
  applyScopeToggle('agents', scope, {
    hints: {
      custom: 'Custom: edit the toggles below, then Save to write this project’s override.',
      global: 'Follow global: this project inherits the global agent settings. The toggles below show the inherited values (read-only).',
    },
    onLock: (locked) => setAgentsFieldsDisabled(locked),
  });
}

function setAgentsFieldsDisabled(disabled) {
  document.querySelectorAll('#agents-tbl input[type="checkbox"]').forEach(cb => { cb.disabled = disabled; });
  const save = document.getElementById('agents-save-btn');
  if (save) save.disabled = disabled;
}

function renderAgents(agents) {
  AGENTS_STATE = agents || [];
  const tbody = document.querySelector('#agents-tbl tbody');
  if (!tbody) return;
  if (!AGENTS_STATE.length) {
    tbody.innerHTML = '<tr><td colspan="4" class="empty">No agents detected.</td></tr>';
    return;
  }
  tbody.innerHTML = AGENTS_STATE.map(a => `
    <tr>
      <td>${escapeHtml(a.name)}</td>
      <td>${escapeHtml(a.kind || '')}</td>
      <td>${a.installed ? 'yes' : 'no'}</td>
      <td><input type="checkbox" class="agents-enable" data-name="${escapeHtml(a.name)}"${a.enabled ? ' checked' : ''}></td>
    </tr>`).join('');
}

async function loadAgentsConfig() {
  const result = document.getElementById('agents-save-result');
  if (result) result.textContent = '';
  try {
    const r = await fetch(`/api/agents/config${scopeProjectQuery()}`);
    if (r.ok) {
      const d = await r.json();
      renderAgents(d.agents || []);
      applyAgentsScope(d.scope === 'custom' ? 'custom' : 'global');
      return;
    }
  } catch (_) { /* fall through */ }
  renderAgents([]);
  applyAgentsScope('global');
}

function readAgentsForm() {
  const enabled = {};
  document.querySelectorAll('#agents-tbl .agents-enable').forEach(cb => {
    enabled[cb.dataset.name] = cb.checked;
  });
  return { enabled };
}

document.getElementById('agents-save-btn').addEventListener('click', async () => {
  const result = document.getElementById('agents-save-result');
  if (result) result.textContent = 'Saving…';
  try {
    const r = await fetch(`/api/agents/config${scopeProjectQuery()}`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(readAgentsForm()),
    });
    const d = await r.json();
    if (!r.ok) throw new Error(d.error || `${r.status}`);
    renderAgents(d.agents || []);
    applyAgentsScope(d.scope === 'custom' ? 'custom' : 'global');
    if (result) result.textContent = '';
    showToast('Agents saved', 'ok');
    pushActivity('Agents config saved');
  } catch (err) {
    if (result) result.innerHTML = `<span style="color:var(--err);">${escapeHtml(err.message || String(err))}</span>`;
  }
});

document.getElementById('agents-scope-global').addEventListener('change', async (ev) => {
  if (!ev.target.checked || !scopeHasProject()) return;
  const result = document.getElementById('agents-save-result');
  if (result) result.textContent = 'Switching to global…';
  try {
    const r = await fetch(scopeClearUrl('/api/agents/config'), {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(readAgentsForm()),
    });
    const d = await r.json();
    if (!r.ok) throw new Error(d.error || `${r.status}`);
    pushActivity('Agents now follow global');
    showToast('Following global agents', 'ok');
    await loadAgentsConfig();
  } catch (err) {
    if (result) result.innerHTML = `<span style="color:var(--err);">${escapeHtml(err.message || String(err))}</span>`;
    applyAgentsScope('custom');
  }
});

document.getElementById('agents-scope-custom').addEventListener('change', (ev) => {
  if (!ev.target.checked || !scopeHasProject()) return;
  applyAgentsScope('custom');
  const result = document.getElementById('agents-save-result');
  if (result) result.textContent = 'Editing project override — click Save to apply.';
});

// Tools — compress
document.getElementById('compress-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const engine = document.getElementById('compress-mode').value;
  const inputText = document.getElementById('compress-input').value;
  const body = {
    text: inputText,
    engine,
    level: document.getElementById('compress-level').value,
    format: document.getElementById('compress-format').value,
    ratio: Number(document.getElementById('compress-ratio').value),
  };
  if (engine === 'llm') {
    const m = document.getElementById('compress-llm-model').value;
    if (m) body.model = m;
  }
  const sum = document.getElementById('compress-summary');
  const compare = document.getElementById('compress-compare');
  const pre = document.getElementById('compress-output');
  const before = document.getElementById('compress-before');
  sum.textContent = 'Compressing…';
  compare.hidden = true;
  const r = await fetch('/api/compress', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(body)});
  if (!r.ok) { sum.innerHTML = `<span style="color:var(--err);">${r.status}: ${await r.text()}</span>`; return; }
  const d = await r.json();
  const origLen = d.original_len ?? inputText.length;
  const compLen = d.compressed_len ?? (d.compressed || '').length;
  const savedChars = d.saved ?? d.saved_chars ?? (origLen - compLen);
  // Prefer server-supplied saved_pct (f64); fall back to computed ratio.
  const savePct = d.saved_pct != null ? Number(d.saved_pct).toFixed(1) :
    (origLen ? (100 - (compLen / origLen) * 100).toFixed(1) : '0');
  const modelNote = d.model ? ` · model=${d.model}` : '';
  sum.innerHTML =
    `<span class="savings-badge">↓ ${savePct}% saved</span>` +
    ` <span style="margin-left:0.5rem;color:var(--muted);">${origLen} chars → ${compLen} chars · mode=${d.mode || engine}${modelNote}</span>`;
  before.textContent = inputText;
  pre.textContent = d.compressed;
  compare.hidden = false;
  pushActivity(`compress · -${savePct}% (-${savedChars} chars)`);
};
document.getElementById('proxy-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const body = {
    mode: document.getElementById('proxy-mode').value,
    command: document.getElementById('proxy-command').value || null,
    raw: document.getElementById('proxy-raw').value,
    context: Number(document.getElementById('proxy-context').value) || 3,
  };
  const sum = document.getElementById('proxy-summary');
  const pre = document.getElementById('proxy-output');
  sum.textContent = 'Filtering…'; pre.hidden = true;
  const r = await fetch('/api/proxy', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(body)});
  if (!r.ok) { sum.innerHTML = `<span style="color:var(--err);">${r.status}: ${await r.text()}</span>`; return; }
  const d = await r.json();
  // saved_pct from server is the authoritative number; fall back to computed.
  const proxyPct = d.saved_pct != null ? Number(d.saved_pct).toFixed(1) :
    (d.original_len ? (100 - (d.filtered_len / d.original_len) * 100).toFixed(1) : '0');
  sum.innerHTML = `<span class="badge ok">✓ ${d.original_len} → ${d.filtered_len}</span>`
    + ` <span class="badge save">−${proxyPct}%</span>`
    + ` <span style="margin-left:0.4rem;color:var(--muted);">saved ${d.saved_chars} chars · mode=${d.mode}</span>`;
  pre.hidden = false; pre.textContent = d.filtered;
};
document.getElementById('diagnose-form').onsubmit = async (ev) => {
  ev.preventDefault();
  if (isGlobalScope()) { showToast(GLOBAL_SCOPE_MESSAGE, 'err'); refreshDiagnoseScope(); return; }
  if (!currentProject()) { showToast('Select or add a project', 'err'); return; }
  if (!projectPath()) { showToast('No path set — add one in Edit project', 'err'); refreshDiagnoseScope(); return; }
  const body = {
    model: document.getElementById('diagnose-model').value,
    raw: document.getElementById('diagnose-raw').value,
    context: Number(document.getElementById('diagnose-context').value) || 3,
  };
  const meta = document.getElementById('diagnose-meta');
  const pre = document.getElementById('diagnose-output');
  meta.textContent = 'Diagnosing…'; pre.hidden = true;
  const r = await fetch('/api/diagnose', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(body)});
  if (!r.ok) {
    const text = await r.text();
    showToast(`Diagnosis failed ${r.status}: ${text}`, 'err');
    meta.innerHTML = `<span style="color:var(--err);">${r.status}: ${text}</span>`;
    return;
  }
  const d = await r.json();
  meta.innerHTML = `<span class="badge ok">${d.provider}/${d.model}</span> in ${d.input_tokens} · out ${d.output_tokens}`;
  pre.hidden = false; pre.textContent = d.diagnosis;
  pushActivity(`diagnose · ${currentProject()} · ${d.provider}`);
};
document.getElementById('repomap-form').onsubmit = async (ev) => {
  ev.preventDefault();
  if (isGlobalScope()) { showToast(GLOBAL_SCOPE_MESSAGE, 'err'); refreshRepomapScope(); return; }
  if (!currentProject()) { showToast('Select or add a project', 'err'); return; }
  const root = projectPath();
  if (!root) { showToast('No path set — add one in Edit project', 'err'); refreshRepomapScope(); return; }
  const body = {
    root,
    ext: document.getElementById('repomap-ext').value || '',
    max_bytes: Number(document.getElementById('repomap-max').value) || 524288,
  };
  const sum = document.getElementById('repomap-summary');
  const pre = document.getElementById('repomap-output');
  sum.textContent = 'Scanning…'; pre.hidden = true;
  const r = await fetch('/api/repo-map', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(body)});
  if (!r.ok) {
    const text = await r.text();
    showToast(`Failed to build repo map ${r.status}: ${text}`, 'err');
    sum.innerHTML = `<span style="color:var(--err);">${r.status}: ${text}</span>`;
    return;
  }
  const d = await r.json();
  sum.innerHTML = `<span class="badge ok">${d.files} files</span> ${d.total_bytes} bytes scanned · signatures ${d.signature_chars} chars`;
  pre.hidden = false;
  pre.textContent = d.entries.map(e => `// ${e.path} [${e.language || ''}]\n${e.signatures}\n`).join('\n');
};

// Templates
const CAT_LABEL = {
  development: 'Development',
  design: 'Design',
  planning: 'Planning',
  scaffold: 'scaffold',
  'doc-chain': 'doc-chain',
  custom: 'custom',
};
let LOADED_TEMPLATES = [];
let TEMPLATE_EDITOR_STATE = { isNew: true, originalName: '', sourceName: '' };
let TEMPLATE_WORKING = null;
let TEMPLATE_SELECTED_FILE_PATH = '';
const TEMPLATE_SLUG_RE = /^[a-z0-9_-]+$/;
const TEMPLATE_VARIABLE_RE = /^\S+$/;
const TEMPLATE_CATEGORY_VALUES = ['scaffold', 'doc-chain', 'custom'];
const TEMPLATE_DOC_CHAIN_NAMES = new Set(['dev', 'design', 'plan']);
const TEMPLATE_PROJECT_KEYWORDS = ['scaffold', 'project', 'standardization', 'contract', 'starter', 'cli', 'lib', 'service', 'rust', 'node', 'python', 'go', 'typescript'];
const TEMPLATE_TREE_INDENT_REM = 1.05;
const TEMPLATE_TREE_BASE_PAD_REM = 0.35;
const TEMPLATE_DUPLICATE_START_INDEX = 2;
const TEMPLATE_PREVIEW_TARGET_PREFIX = './preview-';

function templateSource(t) {
  return String(t.source || '').toLowerCase().replace(/_/g, '');
}
function isBuiltinTemplate(t) {
  return templateSource(t) === 'builtin';
}
function isCustomTemplate(t) {
  return templateSource(t) === 'custom';
}
function categoryLabel(cat) {
  return CAT_LABEL[cat] || cat || 'custom';
}
function templateKind(t) {
  const name = String(t.name || '').toLowerCase();
  if (name === 'standardization') return 'project';
  if (TEMPLATE_DOC_CHAIN_NAMES.has(name)) return 'doc-chain';
  if (String(t.category || '').toLowerCase() === 'development') return 'project';
  const text = `${t.category || ''} ${t.description || ''}`.toLowerCase();
  return TEMPLATE_PROJECT_KEYWORDS.some(keyword => text.includes(keyword)) ? 'project' : 'doc-chain';
}
function categoryEditorValue(cat, template = {}) {
  const raw = String(cat || '').toLowerCase();
  if (TEMPLATE_CATEGORY_VALUES.includes(raw)) return raw;
  return templateKind(template) === 'doc-chain' ? 'doc-chain' : 'scaffold';
}
function isSafeTemplateSlug(name) {
  return TEMPLATE_SLUG_RE.test(name || '');
}
function isSafeTemplateVariableName(name) {
  return TEMPLATE_VARIABLE_RE.test(name || '');
}
function isSafeTemplateFilePath(path) {
  if (!path || path.startsWith('/') || path.includes('\\')) return false;
  return path.split('/').every(part => part && part !== '.' && part !== '..');
}
async function loadTemplates() {
  const list = document.getElementById('tpl-list');
  if (list) list.innerHTML = '<div class="tpl-spinner" aria-label="Loading templates"></div>';
  try {
    const r = await fetch('/api/templates');
    if (!r.ok) throw new Error(`${r.status}: ${await r.text()}`);
    LOADED_TEMPLATES = await r.json();
  } catch (e) {
    LOADED_TEMPLATES = [];
    showToast(`Template load failed: ${e.message || e}`, 'err');
  }
  renderTemplateList();
}
function renderTemplateList() {
  const list = document.getElementById('tpl-list');
  if (!list) return;
  if (!LOADED_TEMPLATES.length) {
    list.innerHTML = '<div class="empty">No templates found.</div>';
    return;
  }
  const sorted = LOADED_TEMPLATES.slice().sort((a, b) => String(a.name || '').localeCompare(String(b.name || '')));
  const projectTemplates = sorted.filter(t => templateKind(t) === 'project');
  const documentChains = sorted.filter(t => templateKind(t) === 'doc-chain');
  const hasCustom = sorted.some(isCustomTemplate);
  const customEmpty = hasCustom ? '' : '<div class="empty">No custom templates yet — duplicate a built-in to start, or create one from scratch.</div>';
  list.innerHTML = [
    renderTemplateGroup('Project Templates', projectTemplates),
    renderTemplateGroup('Document Chains', documentChains),
    customEmpty,
  ].join('');
  list.querySelectorAll('[data-tpl-duplicate]').forEach(btn => btn.onclick = () => duplicateTemplate(btn.dataset.tplDuplicate));
  list.querySelectorAll('[data-tpl-edit]').forEach(btn => btn.onclick = () => editTemplate(btn.dataset.tplEdit));
  list.querySelectorAll('[data-tpl-delete]').forEach(btn => btn.onclick = () => deleteTemplate(btn.dataset.tplDelete));
}
function renderTemplateGroup(title, templates) {
  const count = templates.length;
  const body = count
    ? `<div class="tpl-card-grid">${templates.map(templateCard).join('')}</div>`
    : '<div class="empty">No templates in this group.</div>';
  return `<section class="tpl-kind-group">
    <div class="tpl-kind-head"><span>${escapeHtml(title)}</span><span>${count}</span></div>
    ${body}
  </section>`;
}
function templateCard(t) {
  const active = TEMPLATE_EDITOR_STATE.originalName || document.getElementById('tpl-name')?.value || '';
  const builtin = isBuiltinTemplate(t);
  const activeClass = active === t.name ? ' active' : '';
  const badge = builtin ? '<span class="badge warn">read-only</span>' : '<span class="badge ok">custom</span>';
  const actions = builtin
    ? `<button type="button" class="ghost" data-tpl-duplicate="${escapeAttr(t.name)}">Duplicate as custom</button>`
    : `<button type="button" class="ghost" data-tpl-edit="${escapeAttr(t.name)}">Edit</button><button type="button" class="ghost" data-tpl-delete="${escapeAttr(t.name)}">Delete</button>`;
  return `<article class="tpl-list-item${activeClass}">
    <div class="tpl-top">
      <div><div class="tpl-name">${escapeHtml(t.name || '')}</div><div class="tpl-desc">${escapeHtml(t.description || '')}</div></div>
      ${badge}
    </div>
    <div class="hint">${escapeHtml(categoryLabel(t.category))} · ${(t.variables || []).length} variables</div>
    <div class="tpl-actions">${actions}</div>
  </article>`;
}
async function fetchTemplate(name) {
  const r = await fetch(`/api/templates/${encodeURIComponent(name)}`);
  if (!r.ok) throw new Error(`${r.status}: ${await r.text()}`);
  return r.json();
}
function emptyTemplate() {
  return {
    name: '',
    description: '',
    category: 'custom',
    variables: [{ name: 'project_name', description: '', default: '', required: true }],
    files: [{ path: 'README.md', content: '# {{project_name}}\n', executable: false }],
  };
}
function openTemplateEditor(template, state) {
  TEMPLATE_EDITOR_STATE = state;
  TEMPLATE_WORKING = normalizeTemplateForEditor(template);
  TEMPLATE_SELECTED_FILE_PATH = TEMPLATE_WORKING.files[0]?.path || '';
  document.getElementById('tpl-editor-card').hidden = false;
  document.getElementById('tpl-editor-title').textContent = state.isNew ? 'New Template' : `Edit ${TEMPLATE_WORKING.name}`;
  document.getElementById('tpl-editor-mode').textContent = state.isNew ? 'new custom' : 'custom';
  document.getElementById('tpl-original-name').value = state.originalName || '';
  const nameInput = document.getElementById('tpl-name');
  nameInput.value = TEMPLATE_WORKING.name || '';
  nameInput.readOnly = !state.isNew;
  document.getElementById('tpl-description').value = TEMPLATE_WORKING.description || '';
  document.getElementById('tpl-category').value = categoryEditorValue(TEMPLATE_WORKING.category, TEMPLATE_WORKING);
  document.getElementById('tpl-delete').hidden = state.isNew;
  document.getElementById('tpl-duplicate-current').hidden = true;
  document.getElementById('tpl-editor-result').textContent = '';
  renderTemplateVariables(TEMPLATE_WORKING.variables || []);
  renderTemplateFiles();
  validateTemplateSlugUi();
  validateTemplateVariablesUi(false);
  renderTemplateList();
  setTimeout(() => (state.isNew ? nameInput : document.getElementById('tpl-description')).focus(), 0);
}
function normalizeTemplateForEditor(template) {
  return {
    name: template.name || '',
    description: template.description || '',
    category: categoryEditorValue(template.category, template),
    variables: (template.variables || []).map(v => ({
      name: v.name || '',
      default: v.default || '',
      description: v.description || '',
      required: v.required !== false,
    })),
    files: (template.files || []).map(f => ({
      path: f.path || '',
      content: f.content || '',
      executable: Boolean(f.executable),
    })),
  };
}
function closeTemplateEditor() {
  TEMPLATE_EDITOR_STATE = { isNew: true, originalName: '', sourceName: '' };
  TEMPLATE_WORKING = null;
  TEMPLATE_SELECTED_FILE_PATH = '';
  document.getElementById('tpl-editor-card').hidden = true;
  renderTemplateList();
}
function renderTemplateVariables(vars) {
  const body = document.getElementById('tpl-vars-body');
  body.innerHTML = vars.map(v => templateVariableRow(v)).join('');
  body.querySelectorAll('input').forEach(inp => inp.addEventListener('input', () => validateTemplateVariablesUi(false)));
  body.querySelectorAll('[data-var-remove]').forEach(btn => btn.onclick = () => {
    btn.closest('tr').remove();
    validateTemplateVariablesUi(false);
  });
}
function templateVariableRow(v = {}) {
  return `<tr>
    <td><input data-var-name value="${escapeAttr(v.name || '')}" placeholder="project_name"></td>
    <td><input data-var-default value="${escapeAttr(v.default || '')}" placeholder="demo"></td>
    <td><input data-var-description value="${escapeAttr(v.description || '')}" placeholder="Shown in scaffold form"></td>
    <td><label class="tpl-var-required"><input data-var-required type="checkbox" ${v.required !== false ? 'checked' : ''}> Required</label></td>
    <td><button class="ghost" type="button" data-var-remove>Remove</button></td>
  </tr>`;
}
function collectVariableRows(markInvalid = true) {
  const errors = [];
  const variables = Array.from(document.querySelectorAll('#tpl-vars-body tr')).map(row => {
    const nameInput = row.querySelector('[data-var-name]');
    const variable = {
      name: nameInput.value.trim(),
      default: row.querySelector('[data-var-default]').value || undefined,
      description: row.querySelector('[data-var-description]').value || undefined,
      required: row.querySelector('[data-var-required]').checked,
    };
    const valid = isSafeTemplateVariableName(variable.name);
    nameInput.classList.toggle('invalid', markInvalid && !valid);
    if (!valid) errors.push('Variable names are required and cannot contain spaces.');
    return variable;
  }).filter(v => v.name || v.default || v.description);
  return { variables, errors };
}
function validateTemplateVariablesUi(markInvalid = true) {
  const { errors } = collectVariableRows(markInvalid);
  const status = document.getElementById('tpl-var-status');
  status.innerHTML = errors.length ? `<span style="color:var(--err);">${escapeHtml(errors[0])}</span>` : '';
  return errors.length === 0;
}
function renderTemplateFiles() {
  const tree = document.getElementById('tpl-file-tree');
  const editor = document.getElementById('tpl-file-editor');
  const files = TEMPLATE_WORKING?.files || [];
  if (!files.length) {
    tree.innerHTML = '<div class="empty">No files yet.</div>';
    editor.innerHTML = '<div class="empty">Add a file to edit its content.</div>';
    return;
  }
  if (!files.some(f => f.path === TEMPLATE_SELECTED_FILE_PATH)) {
    TEMPLATE_SELECTED_FILE_PATH = files[0].path;
  }
  tree.innerHTML = renderFileTree(files, TEMPLATE_SELECTED_FILE_PATH);
  tree.querySelectorAll('[data-file-select]').forEach(btn => btn.onclick = () => {
    TEMPLATE_SELECTED_FILE_PATH = btn.dataset.fileSelect;
    renderTemplateFiles();
  });
  renderSelectedFileEditor(editor);
}
function renderFileTree(files, selectedPath) {
  const root = { dirs: new Map(), files: [] };
  files.slice().sort((a, b) => String(a.path || '').localeCompare(String(b.path || ''))).forEach(file => {
    const parts = String(file.path || '').split('/').filter(Boolean);
    let node = root;
    parts.slice(0, -1).forEach(part => {
      if (!node.dirs.has(part)) node.dirs.set(part, { dirs: new Map(), files: [] });
      node = node.dirs.get(part);
    });
    node.files.push({ name: parts[parts.length - 1] || file.path, path: file.path });
  });
  return renderTreeNode(root, selectedPath);
}
function renderTreeNode(node, selectedPath, depth = 0) {
  const pad = (TEMPLATE_TREE_BASE_PAD_REM + (depth * TEMPLATE_TREE_INDENT_REM)).toFixed(2);
  const dirs = Array.from(node.dirs.entries()).sort(([a], [b]) => a.localeCompare(b)).map(([name, child]) =>
    `<div class="tpl-tree-row dir" style="padding-left:${pad}rem;"><span>▾</span><span class="tree-label">${escapeHtml(name)}</span></div>` +
    renderTreeNode(child, selectedPath, depth + 1)
  );
  const files = node.files.sort((a, b) => a.name.localeCompare(b.name)).map(file => {
    const active = file.path === selectedPath ? ' active' : '';
    return `<button type="button" class="tpl-tree-row${active}" data-file-select="${escapeAttr(file.path)}" style="padding-left:${pad}rem;"><span>□</span><span class="tree-label">${escapeHtml(file.name)}</span></button>`;
  });
  return dirs.concat(files).join('');
}
function renderSelectedFileEditor(editor) {
  const file = (TEMPLATE_WORKING?.files || []).find(f => f.path === TEMPLATE_SELECTED_FILE_PATH);
  if (!file) {
    editor.innerHTML = '<div class="empty">Select a file.</div>';
    return;
  }
  const placeholders = placeholdersInContent(file.content);
  const placeholderHtml = placeholders.length
    ? placeholders.map(name => `<span class="badge">{{${escapeHtml(name)}}}</span>`).join('')
    : '<span class="hint">none</span>';
  editor.innerHTML = `<div class="tpl-file-head">
      <code>${escapeHtml(file.path)}</code>
      <label class="tpl-var-required"><input id="tpl-file-executable" type="checkbox" ${file.executable ? 'checked' : ''}> Executable</label>
      <button id="tpl-file-remove" class="ghost" type="button">Remove file</button>
    </div>
    <div id="tpl-placeholder-strip" class="tpl-placeholder-strip"><span>Placeholders:</span>${placeholderHtml}</div>
    <textarea id="tpl-file-content" spellcheck="false" placeholder="File content">${escapeHtml(file.content || '')}</textarea>`;
  document.getElementById('tpl-file-content').addEventListener('input', ev => {
    file.content = ev.target.value;
    const livePlaceholders = placeholdersInContent(file.content);
    document.getElementById('tpl-placeholder-strip').innerHTML = '<span>Placeholders:</span>' + (
      livePlaceholders.length
        ? livePlaceholders.map(name => `<span class="badge">{{${escapeHtml(name)}}}</span>`).join('')
        : '<span class="hint">none</span>'
    );
  });
  document.getElementById('tpl-file-executable').addEventListener('change', ev => {
    file.executable = ev.target.checked;
  });
  document.getElementById('tpl-file-remove').onclick = () => removeSelectedTemplateFile(file.path);
}
function placeholdersInContent(content) {
  const names = new Set();
  String(content || '').replace(/{{\s*([A-Za-z0-9_-]+)\s*}}/g, (_match, name) => {
    names.add(name);
    return _match;
  });
  return Array.from(names).sort((a, b) => a.localeCompare(b));
}
function addTemplateFile() {
  if (!TEMPLATE_WORKING) return;
  const path = prompt('File path', 'src/main.rs');
  const cleanPath = String(path || '').trim();
  if (!cleanPath) return;
  if (!isSafeTemplateFilePath(cleanPath)) {
    showToast('File path must stay inside the scaffold target.', 'err');
    return;
  }
  if (TEMPLATE_WORKING.files.some(file => file.path === cleanPath)) {
    showToast('A file with that path already exists.', 'err');
    return;
  }
  TEMPLATE_WORKING.files.push({ path: cleanPath, content: '', executable: false });
  TEMPLATE_SELECTED_FILE_PATH = cleanPath;
  renderTemplateFiles();
}
function removeSelectedTemplateFile(path) {
  if (!TEMPLATE_WORKING) return;
  if (!confirm(`Remove file "${path}"?`)) return;
  TEMPLATE_WORKING.files = TEMPLATE_WORKING.files.filter(file => file.path !== path);
  TEMPLATE_SELECTED_FILE_PATH = TEMPLATE_WORKING.files[0]?.path || '';
  renderTemplateFiles();
}
function collectTemplateFromEditor() {
  const name = document.getElementById('tpl-name').value.trim();
  if (!isSafeTemplateSlug(name)) throw new Error('Name must use lowercase letters, numbers, hyphens, or underscores.');
  const { variables, errors } = collectVariableRows(true);
  if (errors.length) throw new Error(errors[0]);
  const files = (TEMPLATE_WORKING?.files || []).map(file => ({
    path: String(file.path || '').trim(),
    content: file.content || '',
    executable: Boolean(file.executable),
  }));
  for (const file of files) {
    if (!isSafeTemplateFilePath(file.path)) throw new Error(`File path must stay inside the scaffold target: ${file.path || '(empty)'}`);
  }
  if (!files.length) throw new Error('Add at least one file.');
  return {
    name,
    description: document.getElementById('tpl-description').value.trim(),
    category: document.getElementById('tpl-category').value,
    variables,
    files,
  };
}
function validateTemplateSlugUi() {
  const name = document.getElementById('tpl-name').value.trim();
  const status = document.getElementById('tpl-slug-status');
  if (!name) {
    status.textContent = 'Use lowercase letters, numbers, hyphens, or underscores.';
    return false;
  }
  const ok = isSafeTemplateSlug(name);
  status.innerHTML = ok ? '<span class="badge ok">valid slug</span>' : '<span class="badge err">invalid slug</span>';
  return ok;
}
async function duplicateTemplate(name) {
  try {
    const t = await fetchTemplate(name);
    t.name = uniqueDuplicateName(t.name);
    t.category = categoryEditorValue(t.category, t);
    openTemplateEditor(t, { isNew: true, originalName: '', sourceName: name });
  } catch (e) {
    showToast(`Duplicate failed: ${e.message || e}`, 'err');
  }
}
function uniqueDuplicateName(name) {
  const base = `custom-${String(name || 'template').toLowerCase().replace(/[^a-z0-9_-]+/g, '-')}`.replace(/-+$/g, '');
  const names = new Set(LOADED_TEMPLATES.map(t => t.name));
  if (!names.has(base)) return base;
  let index = TEMPLATE_DUPLICATE_START_INDEX;
  let next = `${base}-${index}`;
  while (names.has(next)) {
    index += 1;
    next = `${base}-${index}`;
  }
  return next;
}
async function editTemplate(name) {
  try {
    const t = await fetchTemplate(name);
    if (isBuiltinTemplate(t)) {
      showToast('Built-in templates are read-only. Duplicate as custom first.', 'err');
      return;
    }
    t.category = categoryEditorValue(t.category, t);
    openTemplateEditor(t, { isNew: false, originalName: t.name, sourceName: '' });
  } catch (e) {
    showToast(`Edit failed: ${e.message || e}`, 'err');
  }
}
async function deleteTemplate(name) {
  const ok = confirm(`Delete custom template "${name}"?`);
  if (!ok) return;
  const r = await fetch(`/api/templates/${encodeURIComponent(name)}`, { method: 'DELETE' });
  if (!r.ok) {
    showToast(`Delete failed ${r.status}: ${await r.text()}`, 'err');
    return;
  }
  showToast(`Deleted ${name}`, 'ok');
  if (TEMPLATE_EDITOR_STATE.originalName === name) {
    closeTemplateEditor();
  }
  await loadTemplates();
}
document.getElementById('tpl-new-btn').onclick = () => openTemplateEditor(emptyTemplate(), { isNew: true, originalName: '' });
document.getElementById('tpl-refresh-btn').onclick = loadTemplates;
document.getElementById('tpl-var-add').onclick = () => {
  const { variables } = collectVariableRows(false);
  variables.push({ name: '', default: '', description: '', required: true });
  renderTemplateVariables(variables);
  validateTemplateVariablesUi(false);
};
document.getElementById('tpl-file-add').onclick = addTemplateFile;
document.getElementById('tpl-name').addEventListener('input', validateTemplateSlugUi);
document.getElementById('tpl-delete').onclick = () => {
  const name = TEMPLATE_EDITOR_STATE.originalName || document.getElementById('tpl-name').value.trim();
  if (name) deleteTemplate(name);
};
document.getElementById('tpl-cancel').onclick = closeTemplateEditor;
document.getElementById('tpl-preview-close').onclick = () => document.getElementById('tpl-preview-modal').hidden = true;
document.getElementById('tpl-preview-modal').onclick = (ev) => { if (ev.target.id === 'tpl-preview-modal') document.getElementById('tpl-preview-modal').hidden = true; };
document.getElementById('tpl-preview').onclick = async () => {
  const out = document.getElementById('tpl-editor-result');
  let template;
  try {
    template = collectTemplateFromEditor();
  } catch (e) {
    out.innerHTML = `<span style="color:var(--err);">${escapeHtml(e.message || String(e))}</span>`;
    return;
  }
  const variables = {};
  for (const v of template.variables) variables[v.name] = v.default || '';
  out.textContent = 'Previewing…';
  const r = await fetch('/api/templates/scaffold/preview', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ template: template.name, target: `${TEMPLATE_PREVIEW_TARGET_PREFIX}${template.name}`, variables, overwrite: false, manifest: template }),
  });
  if (!r.ok) {
    out.innerHTML = `<span style="color:var(--err);">${r.status}: ${escapeHtml(await r.text())}</span>`;
    return;
  }
  const d = await r.json();
  renderTemplatePreview(d);
  out.innerHTML = `<span class="badge ok">Preview ready</span> ${d.files.length} files`;
};
function renderTemplatePreview(preview) {
  document.getElementById('tpl-preview-title').textContent = `Preview ${document.getElementById('tpl-name').value.trim() || 'template'}`;
  document.getElementById('tpl-preview-meta').textContent = `${String(preview.root || '')} · ${(preview.files || []).length} files`;
  const paths = (preview.files || []).map(file => ({
    path: previewRelativePath(preview.root, file.path),
    content: `${file.bytes} bytes${file.executable ? ' · executable' : ''}`,
  }));
  document.getElementById('tpl-preview-tree').innerHTML = renderPreviewTree(paths);
  document.getElementById('tpl-preview-modal').hidden = false;
}
function previewRelativePath(root, path) {
  const rootText = String(root || '').replace(/\/+$/g, '');
  const pathText = String(path || '');
  return rootText && pathText.startsWith(`${rootText}/`) ? pathText.slice(rootText.length + 1) : pathText;
}
function renderPreviewTree(files) {
  if (!files.length) return '<div class="empty">No files in preview.</div>';
  const root = { dirs: new Map(), files: [] };
  files.slice().sort((a, b) => a.path.localeCompare(b.path)).forEach(file => {
    const parts = file.path.split('/').filter(Boolean);
    let node = root;
    parts.slice(0, -1).forEach(part => {
      if (!node.dirs.has(part)) node.dirs.set(part, { dirs: new Map(), files: [] });
      node = node.dirs.get(part);
    });
    node.files.push({ name: parts[parts.length - 1] || file.path, meta: file.content });
  });
  return renderPreviewTreeNode(root);
}
function renderPreviewTreeNode(node, depth = 0) {
  const pad = (TEMPLATE_TREE_BASE_PAD_REM + (depth * TEMPLATE_TREE_INDENT_REM)).toFixed(2);
  const dirs = Array.from(node.dirs.entries()).sort(([a], [b]) => a.localeCompare(b)).map(([name, child]) =>
    `<div class="tpl-tree-row dir" style="padding-left:${pad}rem;"><span>▾</span><span class="tree-label">${escapeHtml(name)}</span></div>` +
    renderPreviewTreeNode(child, depth + 1)
  );
  const files = node.files.sort((a, b) => a.name.localeCompare(b.name)).map(file =>
    `<div class="tpl-tree-row" style="padding-left:${pad}rem;"><span>□</span><span class="tree-label">${escapeHtml(file.name)}</span><span class="hint">${escapeHtml(file.meta)}</span></div>`
  );
  return dirs.concat(files).join('');
}
document.getElementById('tpl-editor-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const out = document.getElementById('tpl-editor-result');
  let template;
  try {
    template = collectTemplateFromEditor();
  } catch (e) {
    out.innerHTML = `<span style="color:var(--err);">${escapeHtml(e.message || String(e))}</span>`;
    return;
  }
  const method = TEMPLATE_EDITOR_STATE.isNew ? 'POST' : 'PUT';
  const url = TEMPLATE_EDITOR_STATE.isNew ? '/api/templates' : `/api/templates/${encodeURIComponent(TEMPLATE_EDITOR_STATE.originalName)}`;
  out.textContent = 'Saving…';
  const r = await fetch(url, {
    method,
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(template),
  });
  if (!r.ok) {
    out.innerHTML = `<span style="color:var(--err);">${r.status}: ${escapeHtml(await r.text())}</span>`;
    return;
  }
  const d = await r.json();
  out.innerHTML = `<span class="badge ok">Saved</span> <code>${escapeHtml(d.name || template.name)}</code>`;
  showToast(`Saved ${template.name}`, 'ok');
  TEMPLATE_EDITOR_STATE = { isNew: false, originalName: template.name, sourceName: '' };
  TEMPLATE_WORKING = normalizeTemplateForEditor(template);
  document.getElementById('tpl-name').readOnly = true;
  document.getElementById('tpl-delete').hidden = false;
  await loadTemplates();
};
function openScaffoldModal(name) {
  const tpl = LOADED_TEMPLATES.find(t => t.name === name);
  if (!tpl) return;
  document.getElementById('scaffold-title').textContent = `New ${categoryLabel(tpl.category)} Project`;
  document.getElementById('scaffold-template').value = name;
  document.getElementById('scaffold-target').value = '';
  document.getElementById('scaffold-overwrite').checked = false;
  document.getElementById('scaffold-result').textContent = '';
  renderScaffoldVars();
  document.getElementById('scaffold-modal').hidden = false;
  setTimeout(() => document.getElementById('scaffold-target').focus(), 0);
}
function closeScaffoldModal() { document.getElementById('scaffold-modal').hidden = true; }
document.getElementById('scaffold-close').onclick = closeScaffoldModal;
document.getElementById('scaffold-modal').onclick = (ev) => { if (ev.target.id === 'scaffold-modal') closeScaffoldModal(); };
function renderScaffoldVars() {
  const name = document.getElementById('scaffold-template').value;
  const tpl = LOADED_TEMPLATES.find(t => t.name === name);
  const wrap = document.getElementById('scaffold-vars');
  if (!tpl) { wrap.innerHTML = ''; return; }
  wrap.innerHTML = `<table style="margin-top:0.5rem;"><tbody>` +
    tpl.variables.map(v =>
      `<tr><td style="width:30%;"><code>${escapeHtml(v.name)}</code>${v.required ? ' *' : ''}</td>` +
      `<td><input data-var="${escapeAttr(v.name)}" placeholder="${escapeAttr(v.description || '')}" value="${escapeAttr(v.default || '')}" style="width:100%;"></td></tr>`
    ).join('') + `</tbody></table>`;
}
function buildScaffoldBody() {
  const variables = {};
  document.querySelectorAll('#scaffold-vars [data-var]').forEach(inp => {
    if (inp.value) variables[inp.dataset.var] = inp.value;
  });
  return {
    template: document.getElementById('scaffold-template').value,
    target: document.getElementById('scaffold-target').value,
    variables,
    overwrite: document.getElementById('scaffold-overwrite').checked,
  };
}
document.getElementById('scaffold-preview').onclick = async () => {
  const out = document.getElementById('scaffold-result');
  out.textContent = 'Previewing…';
  const r = await fetch('/api/templates/scaffold/preview', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(buildScaffoldBody())});
  if (!r.ok) { out.innerHTML = `<span style="color:var(--err);">${r.status}: ${await r.text()}</span>`; return; }
  const d = await r.json();
  const rows = d.files.map(f => `<tr><td><code>${escapeHtml(String(f.path))}</code></td><td>${f.bytes}</td></tr>`).join('');
  out.innerHTML = `<div>${escapeHtml(String(d.root))} under ${d.files.length} files to be created</div><table style="margin-top:0.4rem;"><thead><tr><th>Path</th><th>bytes</th></tr></thead><tbody>${rows}</tbody></table>`;
};
document.getElementById('scaffold-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const out = document.getElementById('scaffold-result');
  out.textContent = 'Generating…';
  const r = await fetch('/api/templates/scaffold', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(buildScaffoldBody())});
  if (!r.ok) { out.innerHTML = `<span style="color:var(--err);">${r.status}: ${await r.text()}</span>`; return; }
  const d = await r.json();
  out.innerHTML = `<span class="badge ok">✓ ${d.files_written} File</span> <code>${escapeHtml(String(d.root))}</code>` +
    (d.post_hooks.length ? `<br>post-hooks: ${d.post_hooks.map(h => `<code>${escapeHtml(h)}</code>`).join(', ')}` : '');
  pushActivity(`scaffold · ${d.files_written} File`);
};

// Prompts
async function loadPrompts() {
  let prompts = [];
  try { prompts = await fetch('/api/prompts').then(r => r.ok ? r.json() : []); } catch (_) {}
  const tbody = document.querySelector('#prompts-tbl tbody');
  if (!prompts.length) return;
  tbody.innerHTML = prompts.map(p => {
    const versions = p.versions.map(v => `<a data-name="${p.name}" data-version="${v}" style="margin-right:0.5rem;">v${v}</a>`).join('');
    return `<tr><td><code>${p.name}</code></td><td>v${p.latest}</td><td>${versions}</td></tr>`;
  }).join('');
  tbody.querySelectorAll('a[data-name]').forEach(a => a.onclick = async () => {
    const body = await fetch(`/api/prompts/${a.dataset.name}/${a.dataset.version}`).then(r => r.json());
    const pre = document.getElementById('prompt-body');
    pre.hidden = false;
    pre.textContent = `# ${body.name} v${body.version}\n\n${body.body}`;
  });
}

// Models — fetched once at boot; drives every model <select> in the UI.
let MODELS_CACHE = [];
async function loadModels() {
  try {
    const r = await fetch('/api/models');
    if (!r.ok) return;
    const d = await r.json();
    MODELS_CACHE = Array.isArray(d.models) ? d.models : [];
  } catch (_) { /* backend not yet available — leave cache empty */ }
  populateModelSelects();
}

// Populate every model <select> that is already in the DOM.
// Called after fetch and again whenever the settings page is opened.
function populateModelSelects() {
  const opts = buildModelOptions('');
  ['cfg-ac-model', 'compress-now-model', 'compress-llm-model'].forEach(id => {
    const sel = document.getElementById(id);
    if (!sel) return;
    const current = sel.value;
    sel.innerHTML = opts;
    // Restore previously selected value if it still exists.
    if (current && sel.querySelector(`option[value="${CSS.escape(current)}"]`)) sel.value = current;
  });
  // Embeddings model <select> — populated from installed Ollama models.
  populateEmbModelSelect(document.getElementById('cfg-emb-model')?.value || '');
}

// Installed Ollama models — fetched for the embedding model <select>.
// GET /api/ollama/models → { models: [{name, ...}] }
let OLLAMA_MODELS_CACHE = [];
async function loadOllamaModelsCache() {
  try {
    const r = await fetch('/api/ollama/models');
    if (!r.ok) return; // 404 not compiled / 502 unreachable — leave cache empty
    const d = await r.json();
    OLLAMA_MODELS_CACHE = Array.isArray(d.models) ? d.models : [];
  } catch (_) { /* Ollama not reachable — leave cache empty */ }
  populateEmbModelSelect(document.getElementById('cfg-emb-model')?.value || '');
}

// Build/refresh the embedding model <select> from the Ollama cache.
// `selected` is preserved even if it is not in the installed list (e.g. the
// configured model is not yet pulled) so the saved value never silently drops.
function populateEmbModelSelect(selected) {
  const sel = document.getElementById('cfg-emb-model');
  if (!sel) return;
  const names = OLLAMA_MODELS_CACHE.map(m => m.name).filter(Boolean);
  if (selected && !names.includes(selected)) names.unshift(selected);
  sel.innerHTML = `<option value="">Select a model…</option>` + names.map(n =>
    `<option value="${escapeHtml(n)}"${n === selected ? ' selected' : ''}>${escapeHtml(n)}</option>`
  ).join('');
  sel.value = selected || '';
}

function renderStatuslineSegments(enabled = STATUSLINE_SEGMENTS) {
  const selected = new Set(enabled);
  const wrap = document.getElementById('statusline-segments');
  wrap.innerHTML = STATUSLINE_SEGMENTS.map(segment => `
    <label class="segment-toggle" for="statusline-segment-${segment}">
      <input id="statusline-segment-${segment}" type="checkbox" value="${segment}" ${selected.has(segment) ? 'checked' : ''}>
      <span>${STATUSLINE_SEGMENT_LABELS[segment] || segment}</span>
    </label>
  `).join('');
  document.querySelectorAll('.placeholder-hint.token-hint').forEach(el => {
    el.textContent = `Available tokens: ${STATUSLINE_TOKENS_HINT}`;
  });
}

// Build the `?project=` query for statusline config GET/POST so the per-project
// override in <repo>/.rtrt/config.toml is read/written. Global scope (or no
// project) sends no project, so the global config is used (inherit).
function statuslineProjectQuery() {
  const project = currentProject();
  if (!project || isGlobalProjectValue(project)) return '';
  return `?project=${encodeURIComponent(project)}`;
}

function readStatuslineForm() {
  return {
    enabled_segments: [...document.querySelectorAll('#statusline-segments input[type="checkbox"]:checked')].map(input => input.value),
    format: document.getElementById('statusline-format').value,
    line2_format: document.getElementById('statusline-line2-format').value,
    line3_format: document.getElementById('statusline-line3-format').value,
    codex_check_timeout_ms: Number(document.getElementById('statusline-timeout').value || 200),
  };
}

// True when a specific (non-global) project is selected. The Scope toggle and
// the "Follow global / Custom" behaviour only apply in that case.
function statuslineHasProject() {
  const project = currentProject();
  return !!project && !isGlobalProjectValue(project);
}

// Enable or disable every editable statusline field (segments, the 3 format
// inputs, timeout). Disabled = greyed read-only view used by "Follow global".
function setStatuslineFieldsDisabled(disabled) {
  const ids = ['statusline-format', 'statusline-line2-format', 'statusline-line3-format', 'statusline-timeout'];
  ids.forEach(id => { document.getElementById(id).disabled = disabled; });
  document.querySelectorAll('#statusline-segments input[type="checkbox"]').forEach(cb => { cb.disabled = disabled; });
  document.getElementById('statusline-segments').classList.toggle('statusline-fields-disabled', disabled);
  const saveBtn = document.querySelector('#statusline-form button[type="submit"]');
  if (saveBtn) saveBtn.disabled = disabled;
}

// Reflect the resolved scope in the UI: show/hide the Scope section, set the
// radios, toggle the field-enabled state, and update the source hint.
function applyStatuslineScope(scope) {
  const section = document.getElementById('statusline-scope-section');
  const hint = document.getElementById('statusline-scope-hint');
  const cfgHint = document.getElementById('statusline-config-hint');
  if (!statuslineHasProject()) {
    // Global scope (or no project): no per-project toggle — edit global defaults.
    section.hidden = true;
    setStatuslineFieldsDisabled(false);
    if (cfgHint) cfgHint.textContent = '~/.rtrt/config.toml';
    return;
  }
  section.hidden = false;
  const custom = scope === 'custom';
  document.getElementById('statusline-scope-global').checked = !custom;
  document.getElementById('statusline-scope-custom').checked = custom;
  setStatuslineFieldsDisabled(!custom);
  if (cfgHint) cfgHint.textContent = custom ? '<repo>/.rtrt/config.toml' : '~/.rtrt/config.toml (inherited)';
  if (hint) {
    hint.textContent = custom
      ? 'Custom: edit the fields below, then Save to write this project’s override.'
      : 'Follow global: this project inherits the global statusline. The fields below show the inherited values (read-only).';
  }
}

async function loadStatuslineConfig() {
  const result = document.getElementById('statusline-save-result');
  try {
    const r = await fetch(`/api/statusline/config${statuslineProjectQuery()}`);
    const d = await r.json();
    if (!r.ok) throw new Error(d.error || `${r.status}`);
    renderStatuslineSegments(d.enabled_segments || STATUSLINE_SEGMENTS);
    document.getElementById('statusline-format').value = d.format || '';
    document.getElementById('statusline-line2-format').value = d.line2_format || '';
    document.getElementById('statusline-line3-format').value = d.line3_format || '';
    document.getElementById('statusline-timeout').value = d.codex_check_timeout_ms ?? 200;
    // `scope` is "custom" when this project owns an override, else "global".
    applyStatuslineScope(d.scope === 'custom' ? 'custom' : 'global');
    result.textContent = '';
  } catch (err) {
    renderStatuslineSegments();
    applyStatuslineScope('global');
    result.innerHTML = `<span style="color:var(--err);">${escapeHtml(err.message || String(err))}</span>`;
  }
}

// Scope radio handlers. Follow global clears the project override (server-side)
// and reloads (fields become disabled, showing the inherited global values).
// Custom enables the fields, seeded from the currently-shown global values, so
// the user can edit and Save writes the override.
document.getElementById('statusline-scope-global').addEventListener('change', async (ev) => {
  if (!ev.target.checked || !statuslineHasProject()) return;
  const result = document.getElementById('statusline-save-result');
  result.textContent = 'Switching to global…';
  try {
    const sep = statuslineProjectQuery() ? '&' : '?';
    const r = await fetch(`/api/statusline/config${statuslineProjectQuery()}${sep}scope=global`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(readStatuslineForm()),
    });
    const d = await r.json();
    if (!r.ok) throw new Error(d.error || `${r.status}`);
    pushActivity('Statusline now follows global');
    showToast('Following global statusline', 'ok');
    await loadStatuslineConfig();
    await loadStatuslinePreview();
  } catch (err) {
    result.innerHTML = `<span style="color:var(--err);">${escapeHtml(err.message || String(err))}</span>`;
    // Revert the radio on failure.
    applyStatuslineScope('custom');
  }
});

document.getElementById('statusline-scope-custom').addEventListener('change', (ev) => {
  if (!ev.target.checked || !statuslineHasProject()) return;
  // Enable fields seeded from the currently-shown (inherited) values; nothing is
  // persisted until the user clicks Save.
  applyStatuslineScope('custom');
  document.getElementById('statusline-save-result').textContent = 'Editing project override — click Save to apply.';
});

async function loadStatuslinePreview() {
  const pre = document.getElementById('statusline-preview');
  pre.textContent = 'Loading...';
  try {
    const r = await fetch('/api/statusline/preview');
    const d = await r.json();
    const lines = Array.isArray(d.lines) ? d.lines : [];
    pre.textContent = lines.length ? lines.join('\n') : '(no preview output)';
  } catch (err) {
    pre.textContent = err.message || String(err);
  }
}

async function loadStatuslinePage() {
  renderStatuslineSegments();
  await Promise.all([loadStatuslineConfig(), loadStatuslinePreview()]);
}

document.getElementById('statusline-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const result = document.getElementById('statusline-save-result');
  // In Follow-global mode for a project there is nothing to save — the override
  // is already cleared; saving would (re)write one. Guard against it.
  if (statuslineHasProject() && document.getElementById('statusline-scope-global').checked) {
    result.textContent = 'Following global — switch to Custom to edit this project.';
    return;
  }
  result.textContent = 'Saving...';
  try {
    const r = await fetch(`/api/statusline/config${statuslineProjectQuery()}`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(readStatuslineForm()),
    });
    const d = await r.json();
    if (!r.ok) throw new Error(d.error || `${r.status}`);
    result.innerHTML = '<span class="badge ok">Saved</span>';
    pushActivity('Statusline config saved');
    showToast('Statusline config saved', 'ok');
    await loadStatuslinePreview();
  } catch (err) {
    result.innerHTML = `<span style="color:var(--err);">${escapeHtml(err.message || String(err))}</span>`;
  }
};

// Settings page — config form.
async function loadConfig() {
  const r = await fetch('/api/config');
  if (!r.ok) return;
  const d = await r.json();
  const cap = d.capture || {};
  const ac = d.auto_compress || {};
  const emb = d.embeddings || {};
  const securityDefaultProfile = d.security?.default_profile;
  if (securityDefaultProfile && String(securityDefaultProfile).trim()) {
    GLOBAL_DEFAULT_PROFILE = String(securityDefaultProfile).trim();
  }
  const bool = (v) => !!v;
  document.getElementById('cfg-capture-enabled').checked = bool(cap.enabled);
  document.getElementById('cfg-capture-redact').checked = bool(cap.redact);
  document.getElementById('cfg-capture-dedup').value = cap.dedup_window_sec ?? 60;
  document.getElementById('cfg-ac-enabled').checked = bool(ac.enabled);
  document.getElementById('cfg-ac-base-url').value = ac.base_url || '';
  document.getElementById('cfg-ac-age-sec').value = ac.age_sec ?? 3600;
  document.getElementById('cfg-ac-min-chars').value = ac.min_chars ?? 200;
  document.getElementById('cfg-ac-batch').value = ac.batch ?? 20;
  document.getElementById('cfg-ac-max-tokens').value = ac.max_tokens ?? 1024;
  // Set selected model after populating options.
  const modelSel = document.getElementById('cfg-ac-model');
  if (ac.model) {
    modelSel.innerHTML = buildModelOptions(ac.model);
  }
  // Embeddings section — fields are present only when the backend includes the key.
  document.getElementById('cfg-emb-enabled').checked = bool(emb.enabled);
  populateEmbModelSelect(emb.model || '');
  document.getElementById('cfg-emb-base-url').value = emb.base_url || '';
  await populateSecurityProfileSelect('setting-default-security-profile', GLOBAL_DEFAULT_PROFILE);
  // Global [memory] + [limits] cards live on the same Settings page.
  await loadMemorySettings();
  await loadLimitsConfig();
}

document.getElementById('settings-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const body = {
    capture: {
      enabled: document.getElementById('cfg-capture-enabled').checked,
      redact: document.getElementById('cfg-capture-redact').checked,
      dedup_window_sec: Number(document.getElementById('cfg-capture-dedup').value),
    },
    auto_compress: {
      enabled: document.getElementById('cfg-ac-enabled').checked,
      model: document.getElementById('cfg-ac-model').value || null,
      base_url: document.getElementById('cfg-ac-base-url').value || null,
      age_sec: Number(document.getElementById('cfg-ac-age-sec').value),
      min_chars: Number(document.getElementById('cfg-ac-min-chars').value),
      batch: Number(document.getElementById('cfg-ac-batch').value),
      max_tokens: Number(document.getElementById('cfg-ac-max-tokens').value),
    },
    embeddings: {
      enabled: document.getElementById('cfg-emb-enabled').checked,
      model: document.getElementById('cfg-emb-model').value || null,
      base_url: document.getElementById('cfg-emb-base-url').value || null,
    },
    security: {
      default_profile: document.getElementById('setting-default-security-profile').value,
    },
  };
  const result = document.getElementById('settings-save-result');
  result.textContent = 'Saving…';
  const r = await fetch('/api/config', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  });
  if (!r.ok) {
    result.innerHTML = `<span style="color:var(--err);">${r.status}: ${await r.text()}</span>`;
    return;
  }
  result.innerHTML = '<span class="badge ok">✓ Saved</span>';
  GLOBAL_DEFAULT_PROFILE = body.security.default_profile || GLOBAL_DEFAULT_PROFILE;
  pushActivity('Settings saved');
  showToast('Settings saved', 'ok');
};

// ===========================================================================
// Memory settings (global [memory]: store path + embed_model). Plain global
// settings — no scope toggle. GET/POST /api/memory/settings.
// ===========================================================================
async function loadMemorySettings() {
  try {
    const r = await fetch('/api/memory/settings');
    if (!r.ok) return;
    const d = await r.json();
    const pathEl = document.getElementById('memory-settings-path');
    const modelEl = document.getElementById('memory-settings-embed-model');
    if (pathEl) pathEl.value = d.path || '';
    if (modelEl) modelEl.value = d.embed_model || '';
    const hint = document.getElementById('memory-settings-hint');
    if (hint && d.config_path) hint.textContent = `${d.config_path} [memory]`;
  } catch (_) { /* ignore */ }
}

async function saveMemorySettings() {
  const result = document.getElementById('memory-settings-result');
  const path = document.getElementById('memory-settings-path').value.trim();
  const embed_model = document.getElementById('memory-settings-embed-model').value.trim();
  if (result) result.textContent = 'Saving…';
  try {
    const r = await fetch('/api/memory/settings', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ path, embed_model }),
    });
    const d = await r.json().catch(() => ({}));
    if (!r.ok) throw new Error(d.error || `${r.status}`);
    if (result) result.innerHTML = '<span class="badge ok">✓ Saved · restart to apply path change</span>';
    pushActivity('Memory settings saved');
    showToast('Memory settings saved', 'ok');
  } catch (e) {
    if (result) result.innerHTML = `<span style="color:var(--err);">${escapeHtml(e.message || String(e))}</span>`;
    showToast(`Memory settings error: ${e.message || e}`, 'err');
  }
}

const memorySettingsSaveBtn = document.getElementById('memory-settings-save-btn');
if (memorySettingsSaveBtn) memorySettingsSaveBtn.onclick = saveMemorySettings;

// ===========================================================================
// Daily usage limits (global [limits.<target>]). Plain global setting — a
// table of targets with optional daily_tokens / daily_requests ceilings.
// GET/POST /api/limits/config. Full-replace write.
// ===========================================================================
let LIMITS_STATE = [];

function renderLimitsTable() {
  const tbody = document.querySelector('#limits-tbl tbody');
  if (!tbody) return;
  if (!LIMITS_STATE.length) {
    tbody.innerHTML = '<tr><td colspan="4" class="empty">No limits set.</td></tr>';
    return;
  }
  tbody.innerHTML = LIMITS_STATE.map((row, i) => `
    <tr>
      <td>${escapeHtml(row.target)}</td>
      <td><input type="number" min="0" data-limit-idx="${i}" data-limit-field="daily_tokens" value="${row.daily_tokens ?? ''}" placeholder="—" style="width:120px;"></td>
      <td><input type="number" min="0" data-limit-idx="${i}" data-limit-field="daily_requests" value="${row.daily_requests ?? ''}" placeholder="—" style="width:120px;"></td>
      <td><button class="ghost" type="button" data-limit-remove="${i}">Remove</button></td>
    </tr>`).join('');
  tbody.querySelectorAll('input[data-limit-idx]').forEach(inp => {
    inp.onchange = () => {
      const idx = Number(inp.dataset.limitIdx);
      const field = inp.dataset.limitField;
      const v = inp.value.trim();
      LIMITS_STATE[idx][field] = v === '' ? null : Number(v);
    };
  });
  tbody.querySelectorAll('[data-limit-remove]').forEach(btn => {
    btn.onclick = () => {
      LIMITS_STATE.splice(Number(btn.dataset.limitRemove), 1);
      renderLimitsTable();
    };
  });
}

async function loadLimitsConfig() {
  try {
    const r = await fetch('/api/limits/config');
    if (!r.ok) return;
    const d = await r.json();
    LIMITS_STATE = Array.isArray(d.targets) ? d.targets.map(t => ({
      target: t.target,
      daily_tokens: t.daily_tokens ?? null,
      daily_requests: t.daily_requests ?? null,
    })) : [];
    const hint = document.getElementById('limits-config-hint');
    if (hint && d.path) hint.textContent = `${d.path} [limits]`;
    renderLimitsTable();
  } catch (_) { /* ignore */ }
}

function addLimitTarget() {
  const target = document.getElementById('limits-add-target').value.trim();
  if (!target) { showToast('Enter a target name.', 'err'); return; }
  if (LIMITS_STATE.some(r => r.target === target)) { showToast('That target already exists.', 'err'); return; }
  const tokVal = document.getElementById('limits-add-tokens').value.trim();
  const reqVal = document.getElementById('limits-add-requests').value.trim();
  LIMITS_STATE.push({
    target,
    daily_tokens: tokVal === '' ? null : Number(tokVal),
    daily_requests: reqVal === '' ? null : Number(reqVal),
  });
  document.getElementById('limits-add-target').value = '';
  document.getElementById('limits-add-tokens').value = '';
  document.getElementById('limits-add-requests').value = '';
  renderLimitsTable();
}

async function saveLimitsConfig() {
  const result = document.getElementById('limits-save-result');
  if (result) result.textContent = 'Saving…';
  // Drop rows that pin neither axis so an empty target isn't persisted.
  const targets = LIMITS_STATE
    .filter(r => r.target && (r.daily_tokens != null || r.daily_requests != null))
    .map(r => ({
      target: r.target,
      daily_tokens: r.daily_tokens == null ? null : Number(r.daily_tokens),
      daily_requests: r.daily_requests == null ? null : Number(r.daily_requests),
    }));
  try {
    const r = await fetch('/api/limits/config', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ targets }),
    });
    const d = await r.json().catch(() => ({}));
    if (!r.ok) throw new Error(d.error || `${r.status}`);
    LIMITS_STATE = Array.isArray(d.targets) ? d.targets.map(t => ({
      target: t.target,
      daily_tokens: t.daily_tokens ?? null,
      daily_requests: t.daily_requests ?? null,
    })) : [];
    renderLimitsTable();
    if (result) result.innerHTML = '<span class="badge ok">✓ Saved</span>';
    pushActivity('Daily usage limits saved');
    showToast('Limits saved', 'ok');
  } catch (e) {
    if (result) result.innerHTML = `<span style="color:var(--err);">${escapeHtml(e.message || String(e))}</span>`;
    showToast(`Limits save error: ${e.message || e}`, 'err');
  }
}

const limitsAddBtn = document.getElementById('limits-add-btn');
if (limitsAddBtn) limitsAddBtn.onclick = addLimitTarget;
const limitsSaveBtn = document.getElementById('limits-save-btn');
if (limitsSaveBtn) limitsSaveBtn.onclick = saveLimitsConfig;

// ===========================================================================
// Per-project embeddings override (force semantic memory on/off). Lives on the
// global config's [[projects]] entry (keyed by name), but uses the shared
// "Follow global / Custom (this project)" scope toggle. GET/POST
// /api/embeddings/project?project=X (+ ?scope=global to clear back to None).
// ===========================================================================
function embeddingsProjectLocked() {
  return scopeHasProject() && document.getElementById('embeddings-scope-global').checked;
}

function applyEmbeddingsScope(scope) {
  applyScopeToggle('embeddings', scope, {
    hints: {
      custom: 'Custom: toggle below to write this project’s embeddings override.',
      global: 'Follow global: this project inherits the global embeddings setting. The toggle below shows the inherited value (read-only).',
    },
    onLock: (locked) => {
      const cb = document.getElementById('embeddings-project-enabled');
      if (cb) cb.disabled = locked;
    },
  });
}

function renderEmbeddingsProject(d) {
  const cb = document.getElementById('embeddings-project-enabled');
  if (cb) {
    cb.checked = !!d.enabled;
    cb.disabled = embeddingsProjectLocked();
  }
  const status = document.getElementById('embeddings-project-status');
  if (status) {
    const src = d.custom ? 'this project' : `global default (${d.global_enabled ? 'on' : 'off'})`;
    status.textContent = `Semantic memory ${d.enabled ? 'ON' : 'OFF'} · source: ${src}`;
  }
}

async function loadEmbeddingsProject() {
  if (!scopeHasProject()) {
    applyEmbeddingsScope('global');
    return;
  }
  try {
    const r = await fetch(`/api/embeddings/project${scopeProjectQuery()}`);
    if (r.ok) {
      const d = await r.json();
      applyEmbeddingsScope(d.scope === 'custom' ? 'custom' : 'global');
      renderEmbeddingsProject(d);
      return;
    }
  } catch (_) { /* fall through */ }
  applyEmbeddingsScope('global');
}

async function setEmbeddingsProject(enabled) {
  try {
    const r = await fetch(`/api/embeddings/project${scopeProjectQuery()}`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ enabled }),
    });
    const d = await r.json();
    if (!r.ok) throw new Error(d.error || `${r.status}`);
    applyEmbeddingsScope(d.scope === 'custom' ? 'custom' : 'global');
    renderEmbeddingsProject(d);
    pushActivity(`embeddings · ${currentProject()} · ${enabled ? 'on' : 'off'}`);
  } catch (e) {
    showToast(`Embeddings save error: ${e.message || e}`, 'err');
    await loadEmbeddingsProject();
  }
}

const embeddingsProjectCb = document.getElementById('embeddings-project-enabled');
if (embeddingsProjectCb) embeddingsProjectCb.addEventListener('change', (ev) => {
  if (embeddingsProjectLocked()) return;
  setEmbeddingsProject(ev.target.checked);
});

const embeddingsScopeGlobal = document.getElementById('embeddings-scope-global');
if (embeddingsScopeGlobal) embeddingsScopeGlobal.addEventListener('change', async (ev) => {
  if (!ev.target.checked || !scopeHasProject()) return;
  try {
    const r = await fetch(scopeClearUrl('/api/embeddings/project'), { method: 'POST' });
    const d = await r.json();
    if (!r.ok) throw new Error(d.error || `${r.status}`);
    pushActivity('Embeddings now follow global');
    showToast('Following global embeddings setting', 'ok');
    await loadEmbeddingsProject();
  } catch (err) {
    showToast(`Scope error: ${err.message || err}`, 'err');
    await loadEmbeddingsProject();
  }
});

const embeddingsScopeCustom = document.getElementById('embeddings-scope-custom');
if (embeddingsScopeCustom) embeddingsScopeCustom.addEventListener('change', (ev) => {
  if (!ev.target.checked || !scopeHasProject()) return;
  applyEmbeddingsScope('custom');
  const cb = document.getElementById('embeddings-project-enabled');
  if (cb) cb.disabled = false;
});

// ===========================================================================
// Per-project bound security profile (else global default_profile). Lives on
// the global config's [[projects]] entry (keyed by name), uses the shared scope
// toggle. GET/POST /api/security/project?project=X (+ ?scope=global to clear).
// ===========================================================================
function securityProfileLocked() {
  return scopeHasProject() && document.getElementById('security-profile-scope-global').checked;
}

function applySecurityProfileScope(scope) {
  applyScopeToggle('security-profile', scope, {
    hints: {
      custom: 'Custom: pick a profile below, then Save to bind it to this project.',
      global: 'Follow global: this project uses the global default profile. The select below shows the inherited value (read-only).',
    },
    onLock: (locked) => {
      const sel = document.getElementById('security-project-profile-select');
      if (sel) sel.disabled = locked;
      const save = document.getElementById('security-project-profile-save-btn');
      if (save) save.disabled = locked;
    },
  });
}

function renderSecurityProjectProfile(d) {
  const status = document.getElementById('security-project-profile-status');
  if (status) {
    status.textContent = d.custom
      ? `Bound profile: ${d.profile}`
      : `Follows global default: ${d.default_profile}`;
  }
}

async function loadSecurityProjectProfile() {
  if (!scopeHasProject()) {
    applySecurityProfileScope('global');
    return;
  }
  try {
    const r = await fetch(`/api/security/project${scopeProjectQuery()}`);
    if (r.ok) {
      const d = await r.json();
      applySecurityProfileScope(d.scope === 'custom' ? 'custom' : 'global');
      await populateSecurityProfileSelect('security-project-profile-select', d.profile);
      const sel = document.getElementById('security-project-profile-select');
      if (sel) sel.disabled = securityProfileLocked();
      renderSecurityProjectProfile(d);
      return;
    }
  } catch (_) { /* fall through */ }
  applySecurityProfileScope('global');
}

async function saveSecurityProjectProfile() {
  if (securityProfileLocked()) return;
  const profile = document.getElementById('security-project-profile-select').value;
  if (!profile) { showToast('Select a profile.', 'err'); return; }
  try {
    const r = await fetch(`/api/security/project${scopeProjectQuery()}`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ profile }),
    });
    const d = await r.json();
    if (!r.ok) throw new Error(d.error || `${r.status}`);
    applySecurityProfileScope(d.scope === 'custom' ? 'custom' : 'global');
    renderSecurityProjectProfile(d);
    // Keep the cached project entry + scan profile select in sync.
    const p = selectedProject();
    if (p) p.security_profile = d.override || null;
    pushActivity(`security profile · ${currentProject()} · ${d.profile}`);
    showToast('Bound profile saved', 'ok');
  } catch (e) {
    showToast(`Bound profile error: ${e.message || e}`, 'err');
    await loadSecurityProjectProfile();
  }
}

const securityProjectProfileSaveBtn = document.getElementById('security-project-profile-save-btn');
if (securityProjectProfileSaveBtn) securityProjectProfileSaveBtn.onclick = saveSecurityProjectProfile;

const securityProfileScopeGlobal = document.getElementById('security-profile-scope-global');
if (securityProfileScopeGlobal) securityProfileScopeGlobal.addEventListener('change', async (ev) => {
  if (!ev.target.checked || !scopeHasProject()) return;
  try {
    const r = await fetch(scopeClearUrl('/api/security/project'), { method: 'POST' });
    const d = await r.json();
    if (!r.ok) throw new Error(d.error || `${r.status}`);
    const p = selectedProject();
    if (p) p.security_profile = null;
    pushActivity('Security profile now follows global');
    showToast('Following global default profile', 'ok');
    await loadSecurityProjectProfile();
  } catch (err) {
    showToast(`Scope error: ${err.message || err}`, 'err');
    await loadSecurityProjectProfile();
  }
});

const securityProfileScopeCustom = document.getElementById('security-profile-scope-custom');
if (securityProfileScopeCustom) securityProfileScopeCustom.addEventListener('change', (ev) => {
  if (!ev.target.checked || !scopeHasProject()) return;
  applySecurityProfileScope('custom');
  const sel = document.getElementById('security-project-profile-select');
  if (sel) sel.disabled = false;
  const save = document.getElementById('security-project-profile-save-btn');
  if (save) save.disabled = false;
});

document.getElementById('compress-now-btn').onclick = async () => {
  const project = document.getElementById('compress-now-project').value.trim();
  if (!project) {
    document.getElementById('compress-now-result').innerHTML = '<span style="color:var(--err);">Enter a project name.</span>';
    return;
  }
  if (isGlobalProjectValue(project)) {
    document.getElementById('compress-now-result').innerHTML = `<span style="color:var(--err);">${GLOBAL_SCOPE_MESSAGE}</span>`;
    return;
  }
  const model = document.getElementById('compress-now-model').value || undefined;
  const result = document.getElementById('compress-now-result');
  result.textContent = 'Compressing…';
  const payload = { project };
  if (model) payload.model = model;
  const r = await fetch('/api/memory/compress', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(payload),
  });
  if (!r.ok) {
    result.innerHTML = `<span style="color:var(--err);">${r.status}: ${await r.text()}</span>`;
    return;
  }
  const d = await r.json();
  result.innerHTML = `<span class="badge ok">✓ Compressed ${d.compressed} · skipped ${d.skipped}</span>`;
  pushActivity(`Compress now · ${project} · ${d.compressed}`);
};

// MCP / connect page settings
document.getElementById('setup-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const body = {
    agent: document.getElementById('setup-agent').value,
    memory: document.getElementById('setup-memory').value || null,
    binary: document.getElementById('setup-binary').value || null,
  };
  const pre = document.getElementById('setup-output');
  pre.hidden = true;
  const r = await fetch('/api/setup', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(body)});
  if (!r.ok) { pre.hidden = false; pre.innerHTML = `<span style="color:var(--err);">${r.status}: ${await r.text()}</span>`; return; }
  const d = await r.json();
  pre.hidden = false;
  pre.textContent = `# ${d.agent} → ${d.target_path}\n\n${d.snippet}`;
};

