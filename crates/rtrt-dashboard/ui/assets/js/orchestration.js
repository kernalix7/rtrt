// Tools › Orchestration — editor for the `[failover]` policy exposed by
// /api/failover/config.
//
// Classic script, loaded after pages.js and before app.js. Depends on
// escapeHtml / showToast / pushActivity / applyScopeToggle / scopeHasProject /
// scopeProjectQuery from the earlier files.

let ORCH_FAILOVER = null;

// A trimmed value, or null when the field was left blank.
function orchBlankToNull(value) {
  const v = (value == null ? '' : String(value)).trim();
  return v === '' ? null : v;
}

// A number input's value, or null when blank/invalid — `null` is what the API
// uses for "derive this rather than pin it".
function orchNumOrNull(value) {
  const v = (value == null ? '' : String(value)).trim();
  if (v === '') return null;
  const n = Number(v);
  return Number.isFinite(n) ? n : null;
}

function orchSplitList(value) {
  return String(value || '')
    .split(',')
    .map(s => s.trim())
    .filter(Boolean);
}

// Marker lists are one-per-line: a marker may legitimately contain a comma.
function orchSplitLines(value) {
  return String(value || '')
    .split('\n')
    .map(s => s.trim())
    .filter(Boolean);
}

function orchClearError() {
  const card = document.getElementById('orch-error-card');
  if (card) card.hidden = true;
}

function orchShowError(message) {
  const card = document.getElementById('orch-error-card');
  const text = document.getElementById('orch-error-text');
  if (text) text.textContent = message;
  if (card) {
    card.hidden = false;
    card.scrollIntoView({ behavior: 'smooth', block: 'nearest' });
  }
}

async function loadOrchestration() {
  try {
    const response = await fetch(`/api/failover/config${scopeProjectQuery()}`);
    ORCH_FAILOVER = response.ok ? await response.json() : null;
  } catch (e) {
    showToast(`Orchestration load error: ${e.message || e}`, 'err');
    return;
  }
  orchClearError();
  applyOrchScope();
  renderOrchestration();
}

function applyOrchScope(state = ORCH_FAILOVER) {
  const custom = !!(state && state.custom);
  applyScopeToggle('team', custom ? 'custom' : 'global', {
    hints: {
      custom: 'Custom: this project carries its own failure policy. Save writes <repo>/.rtrt/config.toml; the global policy is untouched.',
      global: 'Follow global: this project inherits the global failure policy. Save here edits that global policy, for every project.',
    },
  });
  const failHint = document.getElementById('failover-config-hint');
  if (failHint && state && state.path) failHint.textContent = `${state.path} [failover]`;
}

function renderOrchestration() {
  renderOrchFailover();
}

function renderOrchFailover() {
  if (!ORCH_FAILOVER) return;
  const set = (id, value) => { const el = document.getElementById(id); if (el) el.value = value == null ? '' : value; };
  set('orch-fail-fatal', (ORCH_FAILOVER.fatal || []).join('\n'));
  set('orch-fail-quota', (ORCH_FAILOVER.quota || []).join('\n'));
  set('orch-fail-transient', (ORCH_FAILOVER.transient || []).join('\n'));
  set('orch-fail-retries', ORCH_FAILOVER.transient_retries);
  set('orch-fail-divisor', ORCH_FAILOVER.backoff_divisor);
  set('orch-fail-backoff', ORCH_FAILOVER.backoff_ms);
}

async function saveOrchFailover() {
  const result = document.getElementById('orch-fail-save-result');
  const val = (id) => { const el = document.getElementById(id); return el ? el.value : ''; };
  const body = {
    fatal: orchSplitLines(val('orch-fail-fatal')),
    quota: orchSplitLines(val('orch-fail-quota')),
    transient: orchSplitLines(val('orch-fail-transient')),
    transient_retries: orchNumOrNull(val('orch-fail-retries')),
    backoff_divisor: orchNumOrNull(val('orch-fail-divisor')),
    backoff_ms: orchNumOrNull(val('orch-fail-backoff')),
  };
  if (result) result.textContent = 'Saving…';
  try {
    const response = await fetch(`/api/failover/config${scopeProjectQuery()}`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    });
    const data = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(data.error || `${response.status}`);
    ORCH_FAILOVER = data;
    renderOrchFailover();
    applyOrchScope();
    if (result) result.innerHTML = '<span class="badge ok">✓ Saved</span>';
    pushActivity('Orchestration: failure policy saved');
    showToast('Failure policy saved', 'ok');
  } catch (e) {
    if (result) result.innerHTML = `<span style="color:var(--err);">${escapeHtml(e.message || String(e))}</span>`;
    showToast(`Failure policy save error: ${e.message || e}`, 'err');
  }
}

(function wireOrchestrationPage() {
  const on = (id, handler, event = 'click') => {
    const el = document.getElementById(id);
    if (el) el.addEventListener(event, handler);
  };
  on('orch-reload-btn', () => loadOrchestration());
  on('orch-fail-save-btn', saveOrchFailover);
  on('team-scope-global', async (ev) => {
    if (!ev.target.checked || !scopeHasProject()) return;
    try {
      const response = await fetch(scopeClearUrl('/api/failover/config'), { method: 'POST' });
      if (!response.ok) {
        const data = await response.json().catch(() => ({}));
        throw new Error(data.error || `${response.status}`);
      }
      pushActivity('Orchestration now follows global');
      showToast('Following global orchestration', 'ok');
      await loadOrchestration();
    } catch (e) {
      showToast(`Scope error: ${e.message || e}`, 'err');
      applyOrchScope({ custom: true });
    }
  }, 'change');
  on('team-scope-custom', (ev) => {
    if (!ev.target.checked || !scopeHasProject()) return;
    applyOrchScope({ custom: true });
    const result = document.getElementById('orch-fail-save-result');
    if (result) result.textContent = 'Editing project override — click Save to apply.';
  }, 'change');
})();
