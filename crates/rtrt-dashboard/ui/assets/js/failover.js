// Tools › Failover — editor for the `[failover]` policy exposed by
// /api/failover/config.
//
// Classic script, loaded after pages.js and before app.js. Depends on
// escapeHtml / showToast / pushActivity / applyScopeToggle / scopeHasProject /
// scopeProjectQuery from the earlier files.

let FAILOVER_CONFIG = null;

// A number input's value, or null when blank/invalid — `null` is what the API
// uses for "derive this rather than pin it".
function failoverNumOrNull(value) {
  const v = (value == null ? '' : String(value)).trim();
  if (v === '') return null;
  const n = Number(v);
  return Number.isFinite(n) ? n : null;
}

// Marker lists are one-per-line: a marker may legitimately contain a comma.
function failoverSplitLines(value) {
  return String(value || '')
    .split('\n')
    .map(s => s.trim())
    .filter(Boolean);
}

// A failed load must never leave the previous scope's values sitting in an
// editable form: Save would then write them into whatever scope is now active.
function lockFailoverEditor() {
  const save = document.getElementById('failover-save-btn');
  if (save) save.disabled = true;
  document.querySelectorAll('#failover-fields textarea, #failover-fields input[type="number"]').forEach((el) => {
    el.value = '';
    el.disabled = true;
  });
}

async function loadFailover() {
  let response;
  try {
    response = await fetch(`/api/failover/config${scopeProjectQuery()}`);
  } catch (e) {
    FAILOVER_CONFIG = null;
    lockFailoverEditor();
    if (e && e.name !== 'AbortError') showToast(`Failover load error: ${e.message || e}`, 'err');
    return;
  }
  FAILOVER_CONFIG = response.ok ? await response.json().catch(() => null) : null;
  if (!FAILOVER_CONFIG) {
    lockFailoverEditor();
    return;
  }
  applyFailoverScope();
  renderFailover();
}

function applyFailoverScope(state = FAILOVER_CONFIG) {
  const custom = !!(state && state.custom);
  applyScopeToggle('failover', custom ? 'custom' : 'global', {
    hints: {
      custom: 'Custom: this project carries its own failure policy. Save writes <repo>/.rtrt/config.toml; the global policy is untouched.',
      global: 'Follow global: this project inherits the global failure policy. Values below show the inherited policy (read-only).',
    },
    onLock: (locked) => {
      const save = document.getElementById('failover-save-btn');
      if (save) save.disabled = locked;
      document.querySelectorAll('#failover-fields textarea, #failover-fields input[type="number"]').forEach((el) => {
        el.disabled = locked;
      });
    },
  });
  const failHint = document.getElementById('failover-config-hint');
  if (failHint && state && state.path) failHint.textContent = `${state.path} [failover]`;
}

function renderFailover() {
  if (!FAILOVER_CONFIG) return;
  const set = (id, value) => { const el = document.getElementById(id); if (el) el.value = value == null ? '' : value; };
  set('failover-fatal', (FAILOVER_CONFIG.fatal || []).join('\n'));
  set('failover-quota', (FAILOVER_CONFIG.quota || []).join('\n'));
  set('failover-transient', (FAILOVER_CONFIG.transient || []).join('\n'));
  set('failover-retries', FAILOVER_CONFIG.transient_retries);
  set('failover-divisor', FAILOVER_CONFIG.backoff_divisor);
  set('failover-backoff', FAILOVER_CONFIG.backoff_ms);
}

function failoverWriteQuery(scope) {
  const project = scopeProjectQuery();
  if (!scope) return project;
  if (!project) return `?scope=${encodeURIComponent(scope)}`;
  return `${project}&scope=${encodeURIComponent(scope)}`;
}

async function saveFailover() {
  if (!FAILOVER_CONFIG) return;
  if (scopeHasProject() && document.getElementById('failover-scope-global')?.checked) {
    return;
  }
  const result = document.getElementById('failover-save-result');
  const val = (id) => { const el = document.getElementById(id); return el ? el.value : ''; };
  const body = {
    fatal: failoverSplitLines(val('failover-fatal')),
    quota: failoverSplitLines(val('failover-quota')),
    transient: failoverSplitLines(val('failover-transient')),
    transient_retries: failoverNumOrNull(val('failover-retries')),
    backoff_divisor: failoverNumOrNull(val('failover-divisor')),
    backoff_ms: failoverNumOrNull(val('failover-backoff')),
  };
  if (result) result.textContent = 'Saving…';
  try {
    const response = await fetch(`/api/failover/config${failoverWriteQuery(scopeHasProject() ? 'custom' : '')}`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    });
    const data = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(data.error || `${response.status}`);
    FAILOVER_CONFIG = data;
    renderFailover();
    applyFailoverScope();
    if (result) result.innerHTML = '<span class="badge ok">✓ Saved</span>';
    pushActivity('Failover: failure policy saved');
    showToast('Failure policy saved', 'ok');
  } catch (e) {
    if (result) result.innerHTML = `<span style="color:var(--err);">${escapeHtml(e.message || String(e))}</span>`;
    showToast(`Failure policy save error: ${e.message || e}`, 'err');
  }
}

(function wireFailoverPage() {
  const on = (id, handler, event = 'click') => {
    const el = document.getElementById(id);
    if (el) el.addEventListener(event, handler);
  };
  on('failover-reload-btn', () => loadFailover());
  on('failover-save-btn', saveFailover);
  on('failover-scope-global', async (ev) => {
    if (!ev.target.checked || !scopeHasProject()) return;
    try {
      const response = await fetch(scopeClearUrl('/api/failover/config'), { method: 'POST' });
      if (!response.ok) {
        const data = await response.json().catch(() => ({}));
        throw new Error(data.error || `${response.status}`);
      }
      pushActivity('Failover now follows global');
      showToast('Following global failover', 'ok');
      await loadFailover();
    } catch (e) {
      showToast(`Scope error: ${e.message || e}`, 'err');
      applyFailoverScope({ custom: true });
    }
  }, 'change');
  on('failover-scope-custom', (ev) => {
    if (!ev.target.checked || !scopeHasProject()) return;
    applyFailoverScope({ custom: true });
    const result = document.getElementById('failover-save-result');
    if (result) result.textContent = 'Editing project override — click Save to apply.';
  }, 'change');
})();
