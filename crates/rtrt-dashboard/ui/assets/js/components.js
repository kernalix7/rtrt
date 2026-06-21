let CURRENT_PROJECT = null;
const PROJECT_INPUTS = ['recall-project','save-project','blocks-project','block-set-project','export-project','graph-project'];

function openProject(name) {
  if (isGlobalScope() || isGlobalProjectValue(name)) {
    CURRENT_PROJECT = null;
    syncProjectInputs('');
    document.getElementById('mem-empty').textContent = GLOBAL_SCOPE_MESSAGE;
    document.getElementById('mem-empty').hidden = false;
    document.getElementById('mem-detail').hidden = true;
    return;
  }
  if (!name) {
    CURRENT_PROJECT = null;
    document.getElementById('mem-empty').textContent = 'Select or add a project';
    document.getElementById('mem-empty').hidden = false;
    document.getElementById('mem-detail').hidden = true;
    return;
  }
  CURRENT_PROJECT = name;
  syncProjectInputs(name);
  document.getElementById('mem-empty').hidden = true;
  const detail = document.getElementById('mem-detail');
  detail.hidden = false;
  document.getElementById('mem-detail-name').textContent = name;
  const project = selectedProject();
  const memCount = project && project.mem_count !== undefined ? `${project.mem_count} memories` : '';
  const path = project && project.path ? project.path : 'no path set';
  document.getElementById('mem-detail-meta').textContent = [memCount, path].filter(Boolean).join(' · ');
  localStorage.setItem('rtrt.project', name);
  localStorage.setItem('rtrt-project', name);
  // Default sub = history.
  document.querySelectorAll('#memory-subtabs a').forEach(x => x.classList.remove('active'));
  document.querySelector('#memory-subtabs a[data-sub="memhistory"]').classList.add('active');
  document.querySelectorAll('#mem-detail .subpage').forEach(x => x.hidden = true);
  document.getElementById('sub-memhistory').hidden = false;
  loadHistory(name);
}
const HISTORY_PAGE_SIZE = 50;
let HISTORY_OFFSET = 0;
let HISTORY_TOTAL = 0;
let HISTORY_SORT = 'recent';   // 'recent' | 'importance'
let HISTORY_SOURCE_FILTER = 'all'; // 'all' | 'main' | 'subagent'
let SELECTED_IDS = new Set();  // ids of checked rows

function escapeHtml(s) {
  return String(s)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;');
}

// Normalise importance to 0–100 integer.
// Backend returns integer 1–10; legacy/float values ≤1 are treated as 0–1 fractions.
function importancePct(raw) {
  const v = Number(raw);
  return v > 1 ? Math.min(100, Math.round(v * 10)) : Math.min(100, Math.round(v * 100));
}

function timelineSourceKind(item) {
  let kind = null;
  if (item.source_kind !== undefined && item.source_kind !== null) kind = item.source_kind;
  else if (item.metadata && item.metadata.source_kind !== undefined && item.metadata.source_kind !== null) kind = item.metadata.source_kind;
  if (kind === 'main' || kind === 'subagent') return kind;
  return null;
}

function sourceKindBadge(kind) {
  if (kind === 'main') return `<span class="badge">🧠 Main</span>`;
  if (kind === 'subagent') return `<span class="badge source-subagent">🤖 Subagent</span>`;
  return '';
}

function applySourceKindFilter() {
  const list = document.getElementById('history-list');
  const rows = list.querySelectorAll('.hist-item');
  let shown = 0;
  rows.forEach(row => {
    const visible = HISTORY_SOURCE_FILTER === 'all' || row.dataset.sourceKind === HISTORY_SOURCE_FILTER;
    row.style.display = visible ? '' : 'none';
    if (visible) shown++;
  });
  let empty = document.getElementById('history-source-empty');
  if (rows.length && shown === 0) {
    if (!empty) {
      empty = document.createElement('div');
      empty.id = 'history-source-empty';
      empty.className = 'empty';
      list.appendChild(empty);
    }
    empty.textContent = 'No memories for the selected source.';
  } else if (empty) {
    empty.remove();
  }
}

// Render a memory body. Legacy rows (and provider_chat captures) may hold a
// raw JSON payload; pretty-print those in a monospace block. Plain-text
// summaries (the current hook-capture format) render inline escaped.
function renderBody(body) {
  const t = (body || '').trim();
  if (t.startsWith('{') || t.startsWith('[')) {
    try {
      const pretty = JSON.stringify(JSON.parse(t), null, 2);
      return `<pre class="body-json">${escapeHtml(pretty)}</pre>`;
    } catch (_) { /* not valid JSON — fall through */ }
  }
  return escapeHtml(t);
}

// Build a model <select> option list from the shared MODELS_CACHE.
// Includes a blank "default" option so model can be omitted.
function buildModelOptions(selected) {
  const base = `<option value="">default</option>`;
  return base + MODELS_CACHE.map(m =>
    `<option value="${escapeHtml(m.id)}"${m.id === selected ? ' selected' : ''}>${escapeHtml(m.id)} (${escapeHtml(m.source)})</option>`
  ).join('');
}

// Update the bulk action bar visibility/count.
function syncBulkBar() {
  const bar = document.getElementById('bulk-bar');
  const count = SELECTED_IDS.size;
  bar.hidden = count === 0;
  document.getElementById('bulk-count-label').textContent = count;
}

async function loadHistory(name, offset) {
  if (isGlobalScope() || isGlobalProjectValue(name)) { showGlobalScopeEmpty('history-list'); return; }
  if (offset === undefined) offset = 0;
  HISTORY_OFFSET = offset;
  // Reset selection on each page load.
  SELECTED_IDS.clear();
  syncBulkBar();
  const list = document.getElementById('history-list');
  list.innerHTML = '<div class="empty">Loading…</div>';
  // Server-side source filter so main/subagent spans the whole project, not just
  // the current page.
  const skParam = (HISTORY_SOURCE_FILTER && HISTORY_SOURCE_FILTER !== 'all')
    ? `&source_kind=${encodeURIComponent(HISTORY_SOURCE_FILTER)}` : '';
  const r = await fetch(`/api/memory/timeline?project=${encodeURIComponent(name)}&limit=${HISTORY_PAGE_SIZE}&offset=${offset}&sort=${HISTORY_SORT}${skParam}`);
  if (!r.ok) {
    const text = await r.text();
    showToast(`Failed to load memory timeline ${r.status}: ${text}`, 'err');
    list.innerHTML = `<div class="empty" style="color:var(--err);">${r.status}: Failed to load</div>`;
    return;
  }
  const d = await r.json();
  HISTORY_TOTAL = d.total || 0;
  if (!d.items.length) {
    list.innerHTML = `<div class="empty">No memories yet. Use 'Quick save new memory' below to start.</div>`;
    document.getElementById('history-pager').hidden = true;
    document.getElementById('history-meta').textContent = 'All saved permanently · 50 per page';
    return;
  }
  list.innerHTML = d.items.map(i => {
    const srcKind = timelineSourceKind(i);
    const srcBadge = sourceKindBadge(srcKind);
    // Compressed rows: terse body + savings badge + expandable original.
    let compBadge = '', orig = '', recompressRow = '';
    if (i.compressed && i.body_full) {
      const saved = Math.round((1 - i.body.length / i.body_full.length) * 100);
      compBadge = `<span class="kind" style="color:var(--ok)" title="Compressed — recall references this compressed copy">⊟ ${saved}%</span>`;
      orig = `<details class="orig"><summary>Original ${i.body_full.length} chars</summary>${renderBody(i.body_full)}</details>`;
    }
    // Importance badge — rendered when server returns an importance field.
    let impBadge = '';
    if (i.importance !== undefined && i.importance !== null) {
      const pct = importancePct(i.importance);
      impBadge = `<span class="badge imp" title="importance ${pct}%">★ ${pct}%</span>`;
    }
    // Per-row recompress control.
    recompressRow = `<div class="recompress-row" data-proj="${escapeHtml(name)}">
      <select class="row-model-sel">${buildModelOptions('')}</select>
      <button class="ghost row-recompress-btn" type="button" style="font-size:0.8em;">Recompress</button>
      <span class="row-recompress-result" style="font-size:0.8em;color:var(--muted);"></span>
    </div>`;
    return `<div class="hist-item selectable" data-id="${i.id}" data-source-kind="${escapeHtml(srcKind || '')}">
       <input type="checkbox" class="hist-check" data-id="${i.id}" title="Select">
       <div class="hist-click-area" data-id="${i.id}" tabindex="0" role="button" aria-label="View detail">
         <span class="when">${relativeTime(i.created_at)}</span>
         <span class="kind">${escapeHtml(i.kind)}</span>${srcBadge}${compBadge}${impBadge}
         <span class="body">${renderBody(i.body)}${orig}${recompressRow}</span>
       </div>
     </div>`;
  }).join('');
  applySourceKindFilter();

  // Wire checkboxes.
  list.querySelectorAll('.hist-check').forEach(chk => {
    chk.addEventListener('change', () => {
      const id = Number(chk.dataset.id);
      chk.checked ? SELECTED_IDS.add(id) : SELECTED_IDS.delete(id);
      chk.closest('.hist-item').classList.toggle('selected', chk.checked);
      syncBulkBar();
    });
    // Prevent checkbox click from also triggering the detail row click.
    chk.addEventListener('click', ev => ev.stopPropagation());
  });

  // Wire row click → detail modal.
  list.querySelectorAll('.hist-click-area').forEach(area => {
    const open = () => openDetailModal(Number(area.dataset.id));
    area.addEventListener('click', (ev) => {
      // Ignore clicks on the recompress control inside the area.
      if (ev.target.closest('.recompress-row')) return;
      open();
    });
    area.addEventListener('keydown', ev => { if (ev.key === 'Enter' || ev.key === ' ') { ev.preventDefault(); open(); } });
  });

  // Wire per-row recompress buttons.
  list.querySelectorAll('.row-recompress-btn').forEach(btn => {
    btn.onclick = async (ev) => {
      ev.stopPropagation();
      const row = btn.closest('.recompress-row');
      const project = row.dataset.proj;
      if (isGlobalScope() || isGlobalProjectValue(project)) { showToast(GLOBAL_SCOPE_MESSAGE, 'err'); return; }
      const model = row.querySelector('.row-model-sel').value || undefined;
      const result = row.querySelector('.row-recompress-result');
      btn.disabled = true;
      result.textContent = 'Compressing…';
      const payload = { project };
      if (model) payload.model = model;
      try {
        const resp = await fetch('/api/memory/compress', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify(payload),
        });
        if (!resp.ok) { result.textContent = `Error ${resp.status}`; return; }
        const info = await resp.json();
        result.textContent = `✓ Compressed ${info.compressed} · skipped ${info.skipped}`;
        pushActivity(`Recompress · ${project} · ${info.compressed}`);
        loadHistory(project, HISTORY_OFFSET);
      } catch (e) {
        result.textContent = `Error: ${e.message || e}`;
      } finally {
        btn.disabled = false;
      }
    };
  });

  const pager = document.getElementById('history-pager');
  const totalPages = Math.max(1, Math.ceil(HISTORY_TOTAL / HISTORY_PAGE_SIZE));
  const currentPage = Math.floor(offset / HISTORY_PAGE_SIZE) + 1;
  document.getElementById('history-page').textContent = `${currentPage} / ${totalPages} · ${HISTORY_TOTAL} total`;
  document.getElementById('history-prev').disabled = currentPage <= 1;
  document.getElementById('history-next').disabled = currentPage >= totalPages;
  pager.hidden = totalPages <= 1;
  document.getElementById('history-meta').textContent = `${HISTORY_TOTAL} total saved permanently · ${HISTORY_PAGE_SIZE} per page`;
}

// Detail modal: fetch GET /api/memory/{id} and render.
async function openDetailModal(id) {
  // A memory opens by id (scope-independent) — works in global brain too.
  const modal = document.getElementById('mem-detail-modal');
  const body = document.getElementById('detail-modal-body');
  document.getElementById('detail-modal-title').textContent = `Memory #${id}`;
  body.innerHTML = '<div class="empty">Loading…</div>';
  modal.hidden = false;
  let d;
  try {
    const r = await fetch(`/api/memory/${id}`);
    if (r.status === 404) {
      body.innerHTML = '<div class="empty" style="color:var(--err);">Item not found (404)</div>';
      return;
    }
    if (!r.ok) {
      body.innerHTML = `<div class="empty" style="color:var(--err);">Error ${r.status}</div>`;
      return;
    }
    d = await r.json();
  } catch (e) {
    body.innerHTML = `<div class="empty" style="color:var(--err);">Network error: ${e.message || e}</div>`;
    return;
  }
  // Metadata rows from the item's payload/metadata field (object or null).
  const meta = d.metadata || d.payload || {};
  const metaRows = Object.entries(meta).map(([k, v]) =>
    `<tr><td>${escapeHtml(k)}</td><td><code>${escapeHtml(String(v))}</code></td></tr>`
  ).join('') || '<tr><td colspan="2" class="empty" style="font-size:0.82em;">No metadata</td></tr>';

  // Importance display — importancePct() handles both 1–10 integer and 0–1 float scales.
  const impHtml = (d.importance !== undefined && d.importance !== null)
    ? (() => { const pct = importancePct(d.importance); return `<tr><td>Importance</td><td>
        <span class="imp-bar"><span class="imp-bar-fill" style="width:${pct}%"></span></span>
        <span style="margin-left:0.4rem;font-size:0.85em;">${pct}%</span>
       </td></tr>`; })()
    : '';

  body.innerHTML = `
    <table class="detail-meta-table" style="margin-bottom:0.85rem;width:100%;">
      <tr><td>ID</td><td><code>#${d.id}</code></td></tr>
      <tr><td>kind</td><td><code>${escapeHtml(d.kind || '?')}</code></td></tr>
      <tr><td>scope</td><td>${d.scope ? `<code>${escapeHtml(d.scope)}</code>` : '<span style="color:var(--muted);">—</span>'}</td></tr>
      <tr><td>Generate</td><td>${relativeTime(d.created_at)} <span style="color:var(--muted);font-size:0.82em;">(${d.created_at ? new Date(d.created_at*1000).toLocaleString() : '—'})</span></td></tr>
      <tr><td>Compress</td><td>${d.compressed ? '<span class="badge ok">⊟ Compressed</span>' : '<span class="badge">Uncompressed</span>'}</td></tr>
      ${impHtml}
    </table>
    <div style="margin-bottom:0.5rem;font-size:0.82em;color:var(--muted);font-weight:600;">Body</div>
    <div class="detail-body-pre">${escapeHtml(d.body || '')}</div>
    ${d.compressed && d.body_full ? `
      <details style="margin-top:0.6rem;">
        <summary class="muted-summary">View original (${d.body_full.length} chars)</summary>
        <div class="detail-body-pre" style="margin-top:0.4rem;">${escapeHtml(d.body_full)}</div>
      </details>` : ''}
    ${Object.keys(meta).length ? `
      <div style="margin-top:0.85rem;font-size:0.82em;color:var(--muted);font-weight:600;">Metadata</div>
      <table style="margin-top:0.35rem;width:100%;"><tbody>${metaRows}</tbody></table>` : ''}
  `;
}

document.getElementById('detail-modal-close').onclick = () => { document.getElementById('mem-detail-modal').hidden = true; };
document.getElementById('mem-detail-modal').onclick = (ev) => { if (ev.target.id === 'mem-detail-modal') ev.target.hidden = true; };
document.addEventListener('keydown', (ev) => {
  if (ev.key === 'Escape' && !document.getElementById('mem-detail-modal').hidden) {
    document.getElementById('mem-detail-modal').hidden = true;
  }
});
document.getElementById('history-prev').onclick = () => {
  const project = currentProject();
  if (!project || isGlobalScope()) return;
  loadHistory(project, Math.max(0, HISTORY_OFFSET - HISTORY_PAGE_SIZE));
};
document.getElementById('history-next').onclick = () => {
  const project = currentProject();
  if (!project || isGlobalScope()) return;
  loadHistory(project, HISTORY_OFFSET + HISTORY_PAGE_SIZE);
};

// Sort toggle buttons.
document.getElementById('sort-recent').onclick = () => {
  HISTORY_SORT = 'recent';
  document.getElementById('sort-recent').classList.add('active');
  document.getElementById('sort-importance').classList.remove('active');
  const project = currentProject();
  if (project && !isGlobalScope()) loadHistory(project, 0);
};
document.getElementById('sort-importance').onclick = () => {
  HISTORY_SORT = 'importance';
  document.getElementById('sort-importance').classList.add('active');
  document.getElementById('sort-recent').classList.remove('active');
  const project = currentProject();
  if (project && !isGlobalScope()) loadHistory(project, 0);
};
document.querySelectorAll('#source-kind-filter .sort-btn').forEach(btn => {
  btn.onclick = () => {
    HISTORY_SOURCE_FILTER = btn.dataset.sourceKind || 'all';
    document.querySelectorAll('#source-kind-filter .sort-btn').forEach(x => x.classList.toggle('active', x === btn));
    SELECTED_IDS.clear();
    document.querySelectorAll('.hist-check').forEach(c => { c.checked = false; });
    document.querySelectorAll('.hist-item.selected').forEach(i => i.classList.remove('selected'));
    syncBulkBar();
    // Reload from page 0 with the server-side source filter so it spans the
    // whole project rather than only the rows already on screen.
    const project = currentProject();
    if (project && !isGlobalScope()) loadHistory(project, 0);
  };
});

// Bulk delete — confirm dialog before sending.
document.getElementById('bulk-delete-btn').onclick = async () => {
  if (isGlobalScope()) { showToast(GLOBAL_SCOPE_MESSAGE, 'err'); return; }
  if (!SELECTED_IDS.size) return;
  const ids = Array.from(SELECTED_IDS);
  const ok = confirm(`Delete the ${ids.length} selected item(s). This action cannot be undone.`);
  if (!ok) return;
  try {
    const r = await fetch('/api/memory/delete', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ ids }),
    });
    if (r.status === 404) {
      // Endpoint not yet available; hide the bar gracefully.
      pushActivity('Bulk delete API not supported (404)');
      return;
    }
    if (!r.ok) { pushActivity(`Delete error ${r.status}`); return; }
    const d = await r.json();
    pushActivity(`Deleted ${d.deleted}`);
    SELECTED_IDS.clear();
    syncBulkBar();
    const project = currentProject();
    if (project && !isGlobalScope()) loadHistory(project, HISTORY_OFFSET);
  } catch (e) { pushActivity(`Delete error: ${e.message || e}`); }
};
document.getElementById('bulk-clear-btn').onclick = () => {
  SELECTED_IDS.clear();
  document.querySelectorAll('.hist-check').forEach(c => { c.checked = false; });
  document.querySelectorAll('.hist-item.selected').forEach(i => i.classList.remove('selected'));
  syncBulkBar();
};

// Re-load history after save.
const originalSaveHandler = () => {};

const savedProject = localStorage.getItem('rtrt.project') || localStorage.getItem('rtrt-project');
if (savedProject && !isGlobalProjectValue(savedProject)) syncProjectInputs(savedProject);

// Sample data — one-click form fillers so the user can try a tool without
// digging up an example. Each entry is a function so it can vary per call.
const SAMPLES = {
  compress() {
    const ta = document.getElementById('compress-input');
    ta.value = 'To be honest, this bug basically happens in the parser. I think what we should do is actually add more input validation. In other words, we should sanitize all user input by default.';
    pushActivity('Filled compress example');
  },
  proxy() {
    navigate('command', { sub: 'command-proxy' });
    document.getElementById('proxy-command').value = 'cargo build';
    const ta = document.getElementById('proxy-raw');
    ta.value = '   Compiling rtrt-core v0.1.0\n   Compiling rtrt-compress v0.1.0\n   Compiling rtrt-memory v0.1.0\nerror[E0599]: no method named `foo` found for struct `Bar`\n   --> crates/rtrt-memory/src/lib.rs:204:18\n    |\n204 |         store.foo();\n    |               ^^^ method not found\n   Compiling rtrt-providers v0.1.0\nwarning: unused variable `x`\n   --> src/lib.rs:42:9\nerror: could not compile `rtrt-memory` due to previous error';
    pushActivity('Filled filter example');
  },
  diagnose() {
    document.getElementById('diagnose-model').value = 'claude-haiku-4-5';
    const ta = document.getElementById('diagnose-raw');
    ta.value = 'test result: FAILED. 1 passed; 1 failed; 0 ignored\n\nfailures:\n---- tests::roundtrip stdout ----\nthread \'tests::roundtrip\' panicked at \'assertion `left == right` failed\n  left: "hello"\n right: "Hello"\', src/lib.rs:42:9\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace';
    pushActivity('Filled diagnose example');
  },
};
document.querySelectorAll('[data-sample]').forEach(btn => btn.onclick = () => {
  const fn = SAMPLES[btn.dataset.sample];
  if (fn) fn();
});

// Command palette — Cmd+K / Ctrl+K opens. Searches pages + sub-tabs + samples.
const PALETTE_ITEMS = [
  { label: 'Overview · Token Savings', hint: 'overview / optimizer savings', run: () => navigate('overview') },
  { label: 'Memory · Search', hint: 'memory / recall', run: () => navigate('memory', { sub: 'memquery' }) },
  { label: 'Memory · Timeline', hint: 'memory / timeline', run: () => navigate('memory', { sub: 'memhistory' }) },
  { label: 'Memory · Map', hint: 'memory / graph', run: () => navigate('memory', { sub: 'memmap' }) },
  { label: 'Memory · Blocks', hint: 'memory / blocks', run: () => navigate('memory', { sub: 'memblocks' }) },
  { label: 'Memory · Stats', hint: 'memory / stats and manage', run: () => navigate('memory', { sub: 'memstats' }) },
  { label: 'Memory · Backup', hint: 'memory / export', run: () => navigate('memory', { sub: 'membackup' }) },
  { label: 'Add / edit project', hint: 'Project / selector', run: () => openProjectModal(false) },
  { label: 'Output Optimizer · Compress Lite', hint: 'rules / lite', run: () => { navigate('compress', { compressEngine: 'rules', compressLevel: 'lite' }); document.getElementById('compress-input').focus(); } },
  { label: 'Output Optimizer · Compress Full', hint: 'rules / full', run: () => { navigate('compress', { compressEngine: 'rules', compressLevel: 'full' }); document.getElementById('compress-input').focus(); } },
  { label: 'Output Optimizer · Compress Ultra', hint: 'rules / ultra', run: () => { navigate('compress', { compressEngine: 'rules', compressLevel: 'ultra' }); document.getElementById('compress-input').focus(); } },
  { label: 'Output Optimizer · compress_ml', hint: 'ml / compress', run: () => { navigate('compress', { compressEngine: 'ml' }); document.getElementById('compress-input').focus(); } },
  { label: 'Command Optimizer · Gain', hint: 'proxy-run analytics', run: () => navigate('command', { sub: 'command-gain' }) },
  { label: 'Command Optimizer · Coverage', hint: '34 command filters', run: () => navigate('command', { sub: 'command-coverage' }) },
  { label: 'Command Optimizer · Proxy', hint: 'stdin filter demo', run: () => navigate('command', { sub: 'command-proxy' }) },
  { label: 'Command Optimizer · Repo Map', hint: 'repo-map', run: () => navigate('command', { sub: 'command-repomap' }) },
  { label: 'Orchestrate · Environment', hint: 'tools and routing', run: () => navigate('environment') },
  { label: 'Orchestrate · Route', hint: 'dry-run routing', run: () => navigate('route') },
  { label: 'Orchestrate · Providers', hint: 'providers / local models', run: () => navigate('llm') },
  { label: 'Library · Prompts', hint: 'prompts', run: () => navigate('prompts') },
  { label: 'Library · Templates', hint: 'project scaffolds', run: () => navigate('templates') },
  { label: 'Tools · Setup', hint: 'client setup', run: () => navigate('connect') },
  { label: 'Tools · Diagnose', hint: 'diagnose', run: () => navigate('diagnose') },
  { label: 'Tools · Security', hint: 'security', run: () => navigate('security') },
  { label: 'Customize · Statusline', hint: 'rich statusline', run: () => navigate('statusline') },
  { label: 'Customize · Capture / Config', hint: 'config / auto-compress', run: () => navigate('settings') },
  { label: 'Toggle theme', hint: 'dark / light', run: () => document.getElementById('theme-toggle').click() },
  { label: 'Output Optimizer · sample', hint: 'sample · compress', run: () => { navigate('compress'); SAMPLES.compress(); } },
  { label: 'Command Optimizer · sample', hint: 'sample · proxy', run: () => { navigate('command', { sub: 'command-proxy' }); SAMPLES.proxy(); } },
  { label: 'Tools · sample diagnose', hint: 'sample · diagnose', run: () => { navigate('diagnose'); SAMPLES.diagnose(); } },
];
function refreshMemoryScope() {
  openProject(currentProject());
}
function refreshRepomapScope() {
  const ok = setScopeState('repomap-scope-empty', '#sub-command-repomap .card', true);
  const path = projectPath();
  document.getElementById('repomap-project-meta').textContent = ok ? `${currentProject()} · ${path}` : '';
  document.getElementById('repomap-root').value = ok ? path : '';
}
function refreshDiagnoseScope() {
  const ok = setScopeState('diagnose-scope-empty', '#page-diagnose .card', true);
  const path = projectPath();
  document.getElementById('diagnose-project-meta').textContent = ok ? `${currentProject()} · ${path}` : '';
}
function refreshProjectScopePage() {
  const page = activePage();
  if (page === 'memory') refreshMemoryScope();
  if (page === 'security') refreshSecurityScope();
  if (page === 'command') {
    const activeSub = document.querySelector('#command-subtabs a.active');
    if (activeSub && activeSub.dataset.sub === 'command-repomap') refreshRepomapScope();
    if (activeSub && activeSub.dataset.sub === 'command-gain') loadGain();
  }
  if (page === 'diagnose') refreshDiagnoseScope();
  // Statusline reads/writes a per-project override, so reload it on project change.
  if (page === 'statusline') loadStatuslinePage();
  // The Output Optimizer level is also a per-project override, so reload it too.
  if (page === 'compress') { loadOptimizerLevel(); loadCompressionLevel(); }
  // Providers (active + enabled) and Agents enable/disable are per-project too.
  if (page === 'llm') loadProvidersConfig();
  if (page === 'environment') loadAgentsConfig();
}
function setCompressEngine(engine, level) {
  if (!engine) return;
  const select = document.getElementById('compress-mode');
  if (!select) return;
  select.value = engine;
  select.dispatchEvent(new Event('change'));
  if (level) {
    const levelSelect = document.getElementById('compress-level');
    if (levelSelect) levelSelect.value = level;
  }
}
function focusCommandProxy() {
  const form = document.getElementById('proxy-form');
  if (!form) return;
  form.scrollIntoView({ behavior: 'smooth', block: 'start' });
  const raw = document.getElementById('proxy-raw');
  if (raw) raw.focus({ preventScroll: true });
}

function focusOptimizerLevel() {
  const panel = document.querySelector('.optimizer-level-panel');
  if (!panel) return;
  panel.scrollIntoView({ behavior: 'smooth', block: 'start' });
}

function navigate(page, opts = {}) {
  updateGlobalScopeIndicators();
  // Keep the top-level mode switch + visible sidebar in sync with the target
  // page (defined in app.js). A page may be opened programmatically from the
  // other mode (e.g. loadProjects -> 'settings'); this re-parents the chrome.
  if (typeof syncModeForPage === 'function') syncModeForPage(page);
  document.querySelectorAll('aside a.nav').forEach(x => x.classList.remove('active'));
  document.querySelectorAll('.page').forEach(x => x.hidden = true);
  let link = opts.source || null;
  if (!link && opts.sub) link = document.querySelector(`aside a.nav[data-page="${page}"][data-sub="${opts.sub}"]`);
  if (!link && opts.focus) link = document.querySelector(`aside a.nav[data-page="${page}"][data-focus="${opts.focus}"]`);
  if (!link && opts.compressEngine && opts.compressLevel) link = document.querySelector(`aside a.nav[data-page="${page}"][data-compress-engine="${opts.compressEngine}"][data-compress-level="${opts.compressLevel}"]`);
  if (!link && opts.compressEngine) link = document.querySelector(`aside a.nav[data-page="${page}"][data-compress-engine="${opts.compressEngine}"]`);
  if (!link) link = document.querySelector(`aside a.nav[data-page="${page}"]`);
  if (link) link.classList.add('active');
  const target = document.getElementById('page-' + page);
  if (target) target.hidden = false;
  if (page === 'overview') startOverviewPolling();
  else stopOverviewPolling();
  if (page !== 'command') stopGainPolling();
  if (page === 'environment') {
    loadEnvironment();
    loadAgentsConfig();
  }
  if (page === 'memory') {
    refreshMemoryScope();
    if (opts.sub) subClick('memory-subtabs', opts.sub);
  }
  if (page === 'command') {
    if (opts.sub) subClick('command-subtabs', opts.sub);
    const activeSub = document.querySelector('#command-subtabs a.active');
    const sub = activeSub ? activeSub.dataset.sub : 'command-gain';
    if (sub === 'command-gain') startGainPolling();
    if (sub === 'command-coverage') renderCommandCoverage();
    if (sub === 'command-repomap') refreshRepomapScope();
  }
  if (page === 'diagnose') {
    refreshDiagnoseScope();
  }
  if (page === 'settings') {
    updateGlobalScopeIndicators();
    // Reload config each time the panel opens so it stays current.
    populateModelSelects();
    loadOllamaModelsCache(); // refresh embedding model <select> from installed Ollama models
    loadConfig();
  }
  if (page === 'llm') {
    // Load both lists fresh each time the page is opened.
    loadProvidersConfig();
    loadLlmModels();
    loadLlmPs();
  }
  if (page === 'limits') {
    // Tools › Limits — daily usage ceilings (moved out of Capture/Config).
    loadLimitsConfig();
  }
  if (page === 'usage') {
    // Tools › Router — provider usage + headroom + load-balancing decision.
    startUsagePolling();
    refreshUsagePage();
  } else {
    stopUsagePolling();
  }
  if (page === 'security') {
    loadSecurityProfiles();
    refreshSecurityScope();
  }
  if (page === 'statusline') {
    loadStatuslinePage();
  }
  if (page === 'compress') {
    loadOptimizerLevel();
    loadCompressionLevel();
    setCompressEngine(opts.compressEngine, opts.compressLevel);
    if (opts.focus === 'level') focusOptimizerLevel();
    if (opts.focus === 'proxy') focusCommandProxy();
  }
  // Mirror the destination into the address bar (History API). Defined in app.js,
  // which loads after this file; guard so navigate() works even if it's absent.
  if (typeof syncUrl === 'function') syncUrl(page, opts);
}
function subClick(navId, sub) {
  const a = document.querySelector(`#${navId} a[data-sub="${sub}"]`);
  if (a) a.click();
}

const palette = document.getElementById('palette-backdrop');
const paletteInput = document.getElementById('palette-input');
const paletteList = document.getElementById('palette-list');
let paletteIdx = 0;
function renderPalette() {
  const q = paletteInput.value.trim().toLowerCase();
  const items = PALETTE_ITEMS.filter(it => !q || it.label.toLowerCase().includes(q) || it.hint.toLowerCase().includes(q));
  paletteList.innerHTML = items.map((it, i) =>
    `<li data-idx="${i}" class="${i === paletteIdx ? 'active' : ''}">${it.label}<span class="meta">${it.hint}</span></li>`
  ).join('') || '<li class="meta" style="padding:0.75rem;">No results</li>';
  paletteList.dataset.items = JSON.stringify(items.map((_, i) => i));
  paletteList.querySelectorAll('li[data-idx]').forEach(li => li.onclick = () => {
    const idx = Number(li.dataset.idx);
    if (items[idx]) { items[idx].run(); closePalette(); }
  });
}
function openPalette() {
  palette.classList.add('open');
  palette.setAttribute('aria-hidden', 'false');
  paletteInput.value = '';
  paletteIdx = 0;
  renderPalette();
  paletteInput.focus();
}
function closePalette() {
  palette.classList.remove('open');
  palette.setAttribute('aria-hidden', 'true');
}
paletteInput.addEventListener('input', () => { paletteIdx = 0; renderPalette(); });
paletteInput.addEventListener('keydown', (ev) => {
  const items = JSON.parse(paletteList.dataset.items || '[]');
  if (ev.key === 'ArrowDown') { ev.preventDefault(); paletteIdx = (paletteIdx + 1) % Math.max(1, items.length); renderPalette(); }
  else if (ev.key === 'ArrowUp') { ev.preventDefault(); paletteIdx = (paletteIdx - 1 + items.length) % Math.max(1, items.length); renderPalette(); }
  else if (ev.key === 'Enter') {
    const q = paletteInput.value.trim().toLowerCase();
    const filtered = PALETTE_ITEMS.filter(it => !q || it.label.toLowerCase().includes(q) || it.hint.toLowerCase().includes(q));
    if (filtered[paletteIdx]) { filtered[paletteIdx].run(); closePalette(); }
  } else if (ev.key === 'Escape') { closePalette(); }
});
palette.addEventListener('click', (ev) => { if (ev.target === palette) closePalette(); });
document.addEventListener('keydown', (ev) => {
  if ((ev.metaKey || ev.ctrlKey) && ev.key.toLowerCase() === 'k') {
    ev.preventDefault();
    palette.classList.contains('open') ? closePalette() : openPalette();
  }
});

// Activity feed
const FEED = [];
function pushActivity(msg) {
  const t = new Date().toTimeString().slice(0, 8);
  FEED.unshift(`${t} · ${msg}`);
  if (FEED.length > 12) FEED.pop();
  document.getElementById('activity-feed').textContent = FEED[0] || '';
}

const TOAST_DISMISS_MS = 4000;

// Toast notifications — auto-dismiss after 4 s.
function showToast(msg, kind /* 'ok' | 'err' */ = 'ok') {
  const container = document.getElementById('toast-container');
  const div = document.createElement('div');
  div.className = `toast ${kind}`;
  div.textContent = msg;
  container.appendChild(div);
  setTimeout(() => { if (div.parentNode) div.parentNode.removeChild(div); }, TOAST_DISMISS_MS);
}

// Utility
function fmtUsd(v) { return v === null || v === undefined ? '—' : `$${Number(v).toFixed(4)}`; }
function setPill(id, ok, text) {
  const el = document.getElementById(id);
  el.classList.remove('ok', 'warn');
  el.classList.add(ok === true ? 'ok' : ok === false ? 'warn' : '');
  if (text) el.lastChild.textContent = ' ' + text;
}
function animateCount(el, target) {
  if (!el) return;
  const start = Number(el.dataset.target || 0);
  el.dataset.target = target;
  const startAt = performance.now();
  const duration = 600;
  const tick = (now) => {
    const t = Math.min(1, (now - startAt) / duration);
    const eased = 1 - Math.pow(1 - t, 3);
    const value = Math.round(start + (target - start) * eased);
    el.textContent = value.toLocaleString();
    if (t < 1) requestAnimationFrame(tick);
  };
  requestAnimationFrame(tick);
}
function spark(svgId, values, color) {
  const svg = document.getElementById(svgId);
  if (!svg) return;
  if (!values.length) { svg.innerHTML = `<text x="50%" y="55%" text-anchor="middle" fill="var(--muted)" font-size="11">No data</text>`; return; }
  const w = 400, h = 64, pad = 4;
  const max = Math.max(1, ...values);
  const step = values.length > 1 ? (w - pad*2) / (values.length - 1) : 0;
  const pts = values.map((v, i) => {
    const x = pad + i * step;
    const y = h - pad - (v/max) * (h - pad*2);
    return `${x.toFixed(1)},${y.toFixed(1)}`;
  }).join(' ');
  const fill = `${pad},${h-pad} ${pts} ${(pad+(values.length-1)*step).toFixed(1)},${h-pad}`;
  svg.innerHTML =
    `<defs><linearGradient id="g-${svgId}" x1="0" y1="0" x2="0" y2="1">` +
    `<stop offset="0%" stop-color="${color}" stop-opacity="0.35"/>` +
    `<stop offset="100%" stop-color="${color}" stop-opacity="0"/></linearGradient></defs>` +
    `<polygon points="${fill}" fill="url(#g-${svgId})"/>` +
    `<polyline points="${pts}" fill="none" stroke="${color}" stroke-width="1.5"/>` +
    `<text x="${w-pad}" y="11" text-anchor="end" fill="var(--muted)" font-size="10">max ${max}</text>`;
}

