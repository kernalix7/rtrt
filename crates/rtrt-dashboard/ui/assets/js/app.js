// Live activity stream via /api/stream. Each broadcast event nudges the
// overview cards + appends a feed line. Falls back to 5s polling only if
// the EventSource handshake fails (older browsers, proxies stripping SSE).
function subscribeStream() {
  if (typeof EventSource === 'undefined') {
    startOverviewPolling();
    return;
  }
  let es;
  let pollTimer = null;
  const startPolling = () => {
    if (pollTimer) return;
    pollTimer = setInterval(() => {
      if (activePage() === 'overview') loadOverview();
    }, 5000);
  };
  const stopPolling = () => {
    if (!pollTimer) return;
    clearInterval(pollTimer);
    pollTimer = null;
  };
  const connect = () => {
    es = new EventSource('/api/stream');
    es.onopen = () => {
      stopPolling();
      pushActivity('SSE connected · live capture streaming');
    };
    es.onmessage = (ev) => {
      try {
        const d = JSON.parse(ev.data);
        if (d.type === 'memory.save') {
          pushActivity(`memory.save · ${d.kind || '?'} · ${d.project || '?'} (#${d.id})`);
          if (activePage() === 'overview') loadOverview();
        } else if (d.type === 'memory.delete') {
          pushActivity(`memory.delete · #${d.id}`);
          // Reload history if the deleted item belongs to the open project.
          const project = currentProject();
          if (project) loadHistory(project, HISTORY_OFFSET);
        } else if (d.type === 'memory.delete_batch') {
          pushActivity(`memory.delete_batch · ${d.deleted} deleted`);
          const project = currentProject();
          if (project) loadHistory(project, HISTORY_OFFSET);
        } else if (d.type === 'heartbeat') {
          // keep-alive; ignore
        } else {
          pushActivity(`stream · ${d.type || 'event'}`);
        }
      } catch (e) {
        pushActivity(`stream parse: ${e.message || e}`);
      }
    };
    es.onerror = () => {
      startPolling();
      try { es.close(); } catch (_) { /* noop */ }
      setTimeout(connect, 5000);
    };
  };
  connect();
}

// ── Local LLM page (Ollama) ──────────────────────────────────────────────────

// Format raw bytes into a human-readable string (GB / MB / KB).
function humanBytes(n) {
  if (n == null) return '—';
  n = Number(n);
  if (n >= 1e9) return `${(n / 1e9).toFixed(1)} GB`;
  if (n >= 1e6) return `${(n / 1e6).toFixed(1)} MB`;
  if (n >= 1e3) return `${(n / 1e3).toFixed(1)} KB`;
  return `${n} B`;
}

// PATCH /api/config — only the sub-object that changed needs to be sent.
// Backend confirms it merges partial updates, so we don't need to load-then-save-full.
async function setDefaultModel(field /* 'auto_compress' | 'embeddings' */, modelName) {
  // POST /api/config requires full capture + auto_compress; a partial body
  // would 422 or reset other fields to defaults. Load the current config,
  // patch only the one model field, and POST the whole thing back.
  try {
    const cur = await fetch('/api/config');
    if (!cur.ok) { showToast(`Failed to load settings ${cur.status}`, 'err'); return; }
    const cfg = await cur.json();
    const body = {
      capture: cfg.capture,
      auto_compress: { ...cfg.auto_compress },
      embeddings: { ...(cfg.embeddings || {}) },
    };
    body[field] = { ...body[field], model: modelName };
    const r = await fetch('/api/config', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    });
    if (!r.ok) { showToast(`Failed to save settings ${r.status}`, 'err'); return; }
    const label = field === 'auto_compress' ? 'compress default' : 'embedding default';
    showToast(`✓ ${label} Model → ${modelName}`, 'ok');
    pushActivity(`Settings: ${label} model = ${modelName}`);
  } catch (e) { showToast(`Save error: ${e.message || e}`, 'err'); }
}

// Security
const SECURITY_SEVERITIES = ['critical', 'high', 'medium', 'low', 'info'];
const SECURITY_STANDARD_KEYS = ['CWE', 'OWASP', 'NIST', 'ASVS', 'CIS', 'SLSA', 'EU_AI_ACT', 'other'];

function securitySeverity(value) {
  const sev = String(value || 'info').toLowerCase();
  return SECURITY_SEVERITIES.includes(sev) ? sev : 'info';
}

function securitySeverityBadge(sev) {
  const s = securitySeverity(sev);
  return `<span class="badge sev-${s}">${escapeHtml(s)}</span>`;
}

async function securityErrorMessage(r, fallback) {
  let text = '';
  try { text = await r.text(); } catch (_) {}
  if (text) {
    try {
      const parsed = JSON.parse(text);
      text = parsed.error || parsed.message || text;
    } catch (_) {}
  }
  return `${fallback} ${r.status}${text ? `: ${text}` : ''}`;
}

function securityValueList(value) {
  if (value === null || value === undefined || value === '') return [];
  if (Array.isArray(value)) {
    const out = [];
    value.forEach(v => out.push(...securityValueList(v)));
    return out;
  }
  if (typeof value === 'object') {
    const out = [];
    Object.entries(value).forEach(([k, v]) => {
      securityValueList(v).forEach(item => out.push(`${k}: ${item}`));
    });
    return out;
  }
  return [String(value)];
}

function securityStandardsSource(item) {
  if (item && item.standards) return item.standards;
  return {
    CWE: item && (item.CWE || item.cwe),
    OWASP: item && (item.OWASP || item.owasp),
    NIST: item && (item.NIST || item.nist),
    ASVS: item && (item.ASVS || item.asvs),
    CIS: item && (item.CIS || item.cis),
    SLSA: item && (item.SLSA || item.slsa),
    EU_AI_ACT: item && (item.EU_AI_ACT || item.eu_ai_act),
    other: item && item.other,
  };
}

function securityStandardEntries(source) {
  if (!source) return [];
  if (Array.isArray(source)) {
    const out = [];
    source.forEach(v => {
      if (v && typeof v === 'object') out.push(...securityStandardEntries(v));
      else securityValueList(v).forEach(item => out.push(item));
    });
    return out;
  }
  if (typeof source !== 'object') return securityValueList(source);

  const norm = s => String(s).toUpperCase().replace(/[-\s]/g, '_');
  const used = new Set();
  const out = [];
  SECURITY_STANDARD_KEYS.forEach(std => {
    Object.entries(source).forEach(([k, v]) => {
      if (used.has(k) || norm(k) !== norm(std)) return;
      used.add(k);
      securityValueList(v).forEach(item => out.push(`${std}: ${item}`));
    });
  });
  Object.entries(source).forEach(([k, v]) => {
    if (used.has(k)) return;
    securityValueList(v).forEach(item => out.push(`${k}: ${item}`));
  });
  return out;
}

function securityStandardChips(item) {
  const entries = securityStandardEntries(securityStandardsSource(item));
  if (!entries.length) return '<span class="security-chip">No standards</span>';
  return entries.map(s => `<span class="security-chip">${escapeHtml(s)}</span>`).join('');
}

function securityLoc(item) {
  const loc = item.location || {};
  const file = item.file || item.path || item.filename || loc.file || loc.path || '—';
  const line = item.line || item.line_number || loc.line || loc.line_number || '—';
  return `${file}:${line}`;
}

function securityRuleId(item) {
  return item.rule_id || item.rule || item.id || item.name || '—';
}

function securityFindingRow(item) {
  const sev = securitySeverity(item.severity);
  const title = item.title || item.message || item.description || '—';
  const engine = item.engine || item.source || '—';
  const fix = item.fix_hint || item.fix || item.remediation || 'No fix hint';
  return `<div class="security-finding-row">
    <div class="security-finding-main">
      ${securitySeverityBadge(sev)}
      <code>${escapeHtml(securityRuleId(item))}</code>
      <span>${escapeHtml(engine)}</span>
      <code>${escapeHtml(securityLoc(item))}</code>
      <span class="security-finding-title">${escapeHtml(title)}</span>
    </div>
    <details>
      <summary class="muted-summary">Fix hint · standards</summary>
      <div class="security-finding-detail">
        <div>${escapeHtml(fix)}</div>
        <div class="security-standards">${securityStandardChips(item)}</div>
      </div>
    </details>
  </div>`;
}

function securityEngineLabel(item) {
  if (item && typeof item === 'object') {
    const name = item.engine || item.name || item.id || 'engine';
    const reason = item.reason || item.error || item.status || '';
    return reason ? `${name}: ${reason}` : name;
  }
  return String(item);
}

function securityEngineList(items) {
  const list = Array.isArray(items) ? items : [];
  if (!list.length) return '<span class="security-chip">none</span>';
  return list.map(item => `<span class="security-chip">${escapeHtml(securityEngineLabel(item))}</span>`).join('');
}

function securityCounts(findings, data) {
  const raw = data.severity_counts || data.counts || {};
  const counts = {};
  SECURITY_SEVERITIES.forEach(sev => {
    counts[sev] = Number(raw[sev] || raw[sev.toUpperCase()] || 0);
  });
  if (!Object.values(counts).some(Boolean)) {
    findings.forEach(f => { counts[securitySeverity(f.severity)] += 1; });
  }
  return counts;
}

function renderSecurityScan(data) {
  const findings = Array.isArray(data.findings) ? data.findings : (Array.isArray(data.results) ? data.results : []);
  const counts = securityCounts(findings, data);
  const enginesRun = data.engines_run || data.enginesRun || [];
  const enginesSkipped = data.engines_skipped || data.enginesSkipped || [];
  document.getElementById('security-result-meta').textContent = `${findings.length} findings · ${new Date().toLocaleTimeString()}`;

  const summary = `<div class="security-summary">${SECURITY_SEVERITIES.map(sev =>
    `<span class="badge sev-${sev}">${escapeHtml(sev)} <strong>${counts[sev]}</strong></span>`
  ).join('')}</div>`;
  const engines = `<div class="security-engine-lists">
    <div><div class="llm-section-head">engines_run</div><div class="security-standards">${securityEngineList(enginesRun)}</div></div>
    <div><div class="llm-section-head">engines_skipped</div><div class="security-standards">${securityEngineList(enginesSkipped)}</div></div>
  </div>`;

  let grouped = '';
  SECURITY_SEVERITIES.forEach(sev => {
    const rows = findings.filter(f => securitySeverity(f.severity) === sev);
    if (!rows.length) return;
    grouped += `<div class="security-finding-group">
      <h3>${escapeHtml(sev)}</h3>
      ${rows.map(securityFindingRow).join('')}
    </div>`;
  });
  if (!findings.length) grouped = '<div class="empty">No issues ✓</div>';

  document.getElementById('security-results-body').innerHTML = summary + engines + grouped;
}

function normalizeSecurityProfile(data, fallbackName) {
  const profile = data && data.profile ? data.profile : data;
  const rules = Array.isArray(profile) ? profile : (profile && Array.isArray(profile.rules) ? profile.rules : []);
  return {
    name: (profile && profile.name) || fallbackName || '',
    description: (profile && profile.description) || '',
    severity_threshold: (profile && profile.severity_threshold) || 'low',
    exclude: (profile && profile.exclude) || [],
    rules,
  };
}

function renderSecurityProfileSummary(profile) {
  const excludes = securityValueList(profile.exclude);
  const header = `<table style="margin-bottom:0.75rem;"><tbody>
    <tr><td>name</td><td><code>${escapeHtml(profile.name || '—')}</code></td></tr>
    <tr><td>description</td><td>${escapeHtml(profile.description || '—')}</td></tr>
    <tr><td>severity_threshold</td><td><code>${escapeHtml(profile.severity_threshold || '—')}</code></td></tr>
    <tr><td>exclude</td><td>${excludes.length ? excludes.map(x => `<code>${escapeHtml(x)}</code>`).join(' ') : '—'}</td></tr>
  </tbody></table>`;
  if (!profile.rules.length) return header + '<div class="empty">No rules</div>';
  const rows = profile.rules.map(rule => {
    const sev = securitySeverity(rule.severity);
    const title = rule.title || rule.description || rule.message || rule.name || '—';
    const engine = rule.engine || rule.source || '—';
    return `<div class="security-finding-row">
      <div class="security-finding-main">
        ${securitySeverityBadge(sev)}
        <code>${escapeHtml(securityRuleId(rule))}</code>
        <span>${escapeHtml(engine)}</span>
        <span class="security-finding-title">${escapeHtml(title)}</span>
      </div>
      <div class="security-finding-detail">
        <div class="security-standards">${securityStandardChips(rule)}</div>
      </div>
    </div>`;
  }).join('');
  return header + `<div class="security-finding-group">${rows}</div>`;
}

async function loadSecurityProfiles(selected) {
  const bound = selectedProject() && selectedProject().security_profile ? selectedProject().security_profile : GLOBAL_DEFAULT_PROFILE;
  await populateSecurityProfileSelect('security-profile-select', selected || bound);
}

function refreshSecurityScope() {
  updateGlobalScopeIndicators();
  const msg = projectScopeMessage(true);
  const empty = document.getElementById('security-scope-empty');
  const cards = document.querySelectorAll('#sub-securityscan > .card');
  if (empty) {
    empty.textContent = msg || '';
    empty.hidden = !msg;
  }
  cards.forEach(card => { card.hidden = !!msg; });
  const meta = document.getElementById('security-project-meta');
  if (meta) meta.textContent = msg ? '' : `${currentProject()} · ${projectPath()}`;
  if (isGlobalScope()) {
    subClick('security-subtabs', 'securityprofiles');
    loadSecurityGlobalDefaultCard();
    return;
  }
  if (!msg) {
    const p = selectedProject();
    loadSecurityProfiles(p && p.security_profile ? p.security_profile : GLOBAL_DEFAULT_PROFILE);
    loadSecurityProjectProfile();
  } else {
    applySecurityProfileScope('global');
  }
}

async function runSecurityScan() {
  if (isGlobalScope()) { showToast(GLOBAL_SCOPE_MESSAGE, 'err'); refreshSecurityScope(); return; }
  if (!currentProject()) { showToast('Select or add a project', 'err'); refreshSecurityScope(); return; }
  const path = projectPath();
  if (!path) { showToast('No path set — add one in Edit project', 'err'); refreshSecurityScope(); return; }
  const profile = document.getElementById('security-profile-select').value;
  if (!profile) { showToast('Security Select a profile.', 'err'); return; }
  const btn = document.getElementById('security-scan-btn');
  const body = document.getElementById('security-results-body');
  btn.disabled = true;
  document.getElementById('security-result-meta').textContent = 'Scanning…';
  body.innerHTML = '<div class="empty">Scanning…</div>';
  try {
    const r = await fetch('/api/security/scan', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ profile, path }),
    });
    if (!r.ok) {
      const msg = await securityErrorMessage(r, 'Security scan failed');
      showToast(msg, 'err');
      body.innerHTML = `<div class="empty">${escapeHtml(msg)}</div>`;
      return;
    }
    const data = await r.json();
    renderSecurityScan(data);
    showToast('Security scan complete', 'ok');
    pushActivity(`security scan · ${currentProject()} · ${profile} · ${path}`);
  } catch (e) {
    const msg = `Security scan error: ${e.message || e}`;
    showToast(msg, 'err');
    body.innerHTML = `<div class="empty">${escapeHtml(msg)}</div>`;
  } finally {
    btn.disabled = false;
  }
}

async function applySecurityProfileToProject() {
  const name = currentProject();
  const security_profile = document.getElementById('security-profile-select').value;
  if (isGlobalScope()) { showToast(GLOBAL_SCOPE_MESSAGE, 'err'); refreshSecurityScope(); return; }
  if (!name) { showToast('Select or add a project', 'err'); return; }
  if (!security_profile) { showToast('Security Select a profile.', 'err'); return; }
  try {
    const r = await fetch('/api/projects', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name, security_profile }),
    });
    if (!r.ok) { showToast(await securityErrorMessage(r, 'Failed to apply profile'), 'err'); return; }
    const p = selectedProject();
    if (p) p.security_profile = security_profile;
    await loadProjects();
    document.getElementById('project-selector').value = name;
    syncProjectInputs(name);
    showToast('Profile applied', 'ok');
  } catch (e) {
    showToast(`Profile apply error: ${e.message || e}`, 'err');
  }
}

async function fetchSecurityProfile(name) {
  const r = await fetch(`/api/security/profile/${encodeURIComponent(name)}`);
  if (!r.ok) throw new Error(await securityErrorMessage(r, 'Failed to load security profiles'));
  return normalizeSecurityProfile(await r.json(), name);
}

async function viewSecurityProfile() {
  const name = document.getElementById('security-profile-select').value;
  if (!name) { showToast('Security Select a profile.', 'err'); return; }
  const card = document.getElementById('security-profile-card');
  const body = document.getElementById('security-profile-body');
  const meta = document.getElementById('security-profile-meta');
  card.hidden = false;
  meta.textContent = name;
  body.innerHTML = '<div class="empty">Loading…</div>';
  try {
    const profile = await fetchSecurityProfile(name);
    meta.textContent = `${profile.name} · ${profile.rules.length} rules`;
    body.innerHTML = renderSecurityProfileSummary(profile);
  } catch (e) {
    const msg = e.message || e;
    showToast(msg, 'err');
    body.innerHTML = `<div class="empty">${escapeHtml(msg)}</div>`;
  }
}

let ACTIVE_SECURITY_PROFILE = null;

async function loadSecurityGlobalDefaultCard() {
  const card = document.getElementById('security-global-default-card');
  if (card) card.hidden = !isGlobalScope();
  if (!isGlobalScope()) return;
  try {
    const r = await fetch('/api/config');
    if (r.ok) {
      const d = await r.json();
      const securityDefaultProfile = d.security?.default_profile;
      if (securityDefaultProfile && String(securityDefaultProfile).trim()) {
        GLOBAL_DEFAULT_PROFILE = String(securityDefaultProfile).trim();
      }
    }
  } catch (_) { /* keep cached default */ }
  await populateSecurityProfileSelect('security-global-default-profile-select', GLOBAL_DEFAULT_PROFILE);
}

async function saveSecurityGlobalDefaultProfile() {
  if (!isGlobalScope()) return;
  const select = document.getElementById('security-global-default-profile-select');
  const result = document.getElementById('security-global-default-result');
  const defaultProfile = select ? select.value : '';
  if (!defaultProfile) { showToast('Select a global default security profile.', 'err'); return; }
  result.textContent = 'Saving…';
  try {
    const cur = await fetch('/api/config');
    if (!cur.ok) {
      result.innerHTML = `<span style="color:var(--err);">Failed to load settings ${cur.status}</span>`;
      return;
    }
    const body = await cur.json();
    body.security = { ...(body.security || {}), default_profile: defaultProfile };
    const r = await fetch('/api/config', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    });
    if (!r.ok) {
      result.innerHTML = `<span style="color:var(--err);">${r.status}: ${await r.text()}</span>`;
      return;
    }
    GLOBAL_DEFAULT_PROFILE = defaultProfile;
    await populateSecurityProfileSelect('setting-default-security-profile', GLOBAL_DEFAULT_PROFILE);
    result.innerHTML = '<span class="badge ok">✓ Global default saved</span>';
    pushActivity('Global default security profile saved');
    showToast('Global default saved', 'ok');
  } catch (e) {
    result.innerHTML = `<span style="color:var(--err);">Error: ${escapeHtml(e.message || String(e))}</span>`;
  }
}

async function loadSecurityProfileSettings() {
  updateGlobalScopeIndicators();
  if (isGlobalScope()) loadSecurityGlobalDefaultCard();
  const list = document.getElementById('security-profile-list');
  list.innerHTML = '<div class="empty">Loading…</div>';
  const profiles = await fetchSecurityProfiles();
  if (!profiles.length) {
    list.innerHTML = '<div class="empty">No profiles</div>';
    return;
  }
  list.innerHTML = profiles.map(name =>
    `<button class="ghost" type="button" data-profile="${escapeAttr(name)}">${escapeHtml(name)}</button>`
  ).join('');
  list.querySelectorAll('[data-profile]').forEach(btn => {
    btn.onclick = () => loadSecurityProfileDetail(btn.dataset.profile);
  });
  const initial = document.getElementById('security-profile-select').value || profiles[0];
  loadSecurityProfileDetail(initial);
}

async function loadSecurityProfileDetail(name) {
  const detail = document.getElementById('security-profile-detail');
  document.querySelectorAll('#security-profile-list [data-profile]').forEach(btn => {
    btn.classList.toggle('active', btn.dataset.profile === name);
  });
  detail.innerHTML = '<div class="empty">Loading…</div>';
  try {
    ACTIVE_SECURITY_PROFILE = await fetchSecurityProfile(name);
    detail.innerHTML = renderSecurityProfileSummary(ACTIVE_SECURITY_PROFILE);
    document.getElementById('security-profile-new-name').value = `${ACTIVE_SECURITY_PROFILE.name}-copy`;
    document.getElementById('security-profile-toml').value = '';
  } catch (e) {
    ACTIVE_SECURITY_PROFILE = null;
    const msg = e.message || e;
    showToast(msg, 'err');
    detail.innerHTML = `<div class="empty">${escapeHtml(msg)}</div>`;
  }
}

function tomlString(value) {
  return `"${String(value || '').replace(/\\/g, '\\\\').replace(/"/g, '\\"').replace(/\n/g, '\\n')}"`;
}

function tomlValue(value) {
  if (Array.isArray(value)) return `[${value.map(tomlValue).join(', ')}]`;
  if (typeof value === 'number' || typeof value === 'boolean') return String(value);
  return tomlString(value);
}

function securityProfileToToml(profile, newName) {
  const lines = [
    '[profile]',
    `name = ${tomlString(newName || profile.name)}`,
    `description = ${tomlString(profile.description || '')}`,
    `severity_threshold = ${tomlString(profile.severity_threshold || 'low')}`,
  ];
  if (profile.exclude && profile.exclude.length) lines.push(`exclude = ${tomlValue(profile.exclude)}`);
  const ruleFields = ['id', 'rule_id', 'name', 'title', 'description', 'severity', 'engine', 'source', 'pattern', 'path', 'message', 'fix_hint'];
  profile.rules.forEach(rule => {
    lines.push('', '[[rules]]');
    ruleFields.forEach(key => {
      if (rule[key] !== undefined && rule[key] !== null && rule[key] !== '') lines.push(`${key} = ${tomlValue(rule[key])}`);
    });
  });
  return lines.join('\n') + '\n';
}

function cloneSecurityProfileToml() {
  if (!ACTIVE_SECURITY_PROFILE) { showToast('Select a profile.', 'err'); return; }
  const nameInput = document.getElementById('security-profile-new-name');
  const newName = nameInput.value.trim() || `${ACTIVE_SECURITY_PROFILE.name}-copy`;
  nameInput.value = newName;
  document.getElementById('security-profile-toml').value = securityProfileToToml(ACTIVE_SECURITY_PROFILE, newName);
}

async function saveSecurityProfileToml() {
  const name = document.getElementById('security-profile-new-name').value.trim();
  const toml = document.getElementById('security-profile-toml').value;
  if (!name) { showToast('Enter a new profile name.', 'err'); return; }
  if (!toml.trim()) { showToast('No TOML to save. Clone a profile first.', 'err'); return; }
  try {
    const r = await fetch('/api/security/profile', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name, toml }),
    });
    if (!r.ok) { showToast(await securityErrorMessage(r, 'Failed to save profile'), 'err'); return; }
    showToast('Profile saved', 'ok');
    await loadSecurityProfiles();
    await loadSecurityProfileSettings();
  } catch (e) {
    showToast(`Profile save error: ${e.message || e}`, 'err');
  }
}

document.getElementById('security-scan-btn').onclick = runSecurityScan;
document.getElementById('security-view-profile-btn').onclick = viewSecurityProfile;
document.getElementById('security-apply-profile-btn').onclick = applySecurityProfileToProject;
document.getElementById('security-profile-clone-btn').onclick = cloneSecurityProfileToml;
document.getElementById('security-profile-save-btn').onclick = saveSecurityProfileToml;
document.getElementById('security-global-default-save-btn').onclick = saveSecurityGlobalDefaultProfile;

// GET /api/ollama/models → { models: [{name, size, modified_at, digest}] }
async function loadLlmModels() {
  const body = document.getElementById('llm-models-body');
  body.innerHTML = '<div class="empty">Loading…</div>';
  let models;
  try {
    const r = await fetch('/api/ollama/models');
    // 502 = Ollama unreachable; 404 = endpoint not compiled in.
    if (r.status === 404 || r.status === 502) {
      body.innerHTML = `<div class="llm-offline">Ollama not reachable (${r.status})</div>`;
      return;
    }
    if (!r.ok) { body.innerHTML = `<div class="llm-offline">Error ${r.status}</div>`; return; }
    const d = await r.json();
    models = d.models || [];
  } catch (e) { body.innerHTML = `<div class="llm-offline">Network error: ${e.message || e}</div>`; return; }

  if (!models.length) { body.innerHTML = '<div class="empty">No installed models. Pull one above.</div>'; return; }

  // `size` (not size_bytes) per confirmed backend shape; no `family` field.
  body.innerHTML = `<table>
    <thead><tr><th>Name</th><th>Size</th><th>Modified</th><th style="text-align:right;">Actions</th></tr></thead>
    <tbody>${models.map(m => {
      const mod = m.modified_at ? new Date(m.modified_at).toLocaleDateString('ko-KR') : '—';
      return `<tr>
        <td><code>${escapeHtml(m.name)}</code></td>
        <td>${humanBytes(m.size)}</td>
        <td style="color:var(--muted);font-size:0.85em;">${mod}</td>
        <td style="text-align:right;white-space:nowrap;">
          <button class="ghost" style="font-size:0.8em;padding:0.2rem 0.5rem;" data-llm-compress="${escapeHtml(m.name)}">Set as compress default</button>
          <button class="ghost" style="font-size:0.8em;padding:0.2rem 0.5rem;" data-llm-embed="${escapeHtml(m.name)}">Set as embedding default</button>
          <button class="ghost" style="font-size:0.8em;padding:0.2rem 0.5rem;color:var(--err);" data-llm-delete="${escapeHtml(m.name)}">Delete</button>
        </td>
      </tr>`;
    }).join('')}</tbody>
  </table>`;

  // Wire per-row buttons.
  body.querySelectorAll('[data-llm-compress]').forEach(btn => {
    btn.onclick = () => setDefaultModel('auto_compress', btn.dataset.llmCompress);
  });
  body.querySelectorAll('[data-llm-embed]').forEach(btn => {
    btn.onclick = () => setDefaultModel('embeddings', btn.dataset.llmEmbed);
  });
  body.querySelectorAll('[data-llm-delete]').forEach(btn => {
    btn.onclick = async () => {
      const name = btn.dataset.llmDelete;
      if (!confirm(`Delete model "${name}". It will be removed from disk completely.`)) return;
      btn.disabled = true;
      try {
        // DELETE /api/ollama/{name} — path param, URL-encode colons.
        const r = await fetch(`/api/ollama/${encodeURIComponent(name)}`, { method: 'DELETE' });
        if (r.status === 404 || r.status === 502) {
          showToast(`Delete failed (${r.status})`, 'err');
          return;
        }
        if (!r.ok) { showToast(`Delete failed ${r.status}`, 'err'); return; }
        showToast(`✓ ${name} deleted`, 'ok');
        pushActivity(`ollama delete · ${name}`);
        loadLlmModels();
      } catch (e) { showToast(`Delete error: ${e.message || e}`, 'err'); }
      finally { btn.disabled = false; }
    };
  });
}

// GET /api/ollama/ps → { models: [{name, size, digest, expires_at}] }
async function loadLlmPs() {
  const body = document.getElementById('llm-ps-body');
  body.innerHTML = '<div class="empty">Loading…</div>';
  let ps;
  try {
    const r = await fetch('/api/ollama/ps');
    if (r.status === 404 || r.status === 502) {
      body.innerHTML = `<div class="llm-offline">Ollama not reachable (${r.status})</div>`;
      return;
    }
    if (!r.ok) { body.innerHTML = `<div class="llm-offline">Error ${r.status}</div>`; return; }
    const d = await r.json();
    ps = d.models || [];
  } catch (e) { body.innerHTML = `<div class="llm-offline">Network error: ${e.message || e}</div>`; return; }

  if (!ps.length) { body.innerHTML = '<div class="empty">No models currently running.</div>'; return; }

  // `size` (not size_bytes), `expires_at` (not until), no size_vram_bytes in this shape.
  body.innerHTML = `<table>
    <thead><tr><th>Name</th><th>Size</th><th>Expires</th></tr></thead>
    <tbody>${ps.map(m => {
      const exp = m.expires_at ? new Date(m.expires_at).toLocaleTimeString() : '—';
      return `<tr>
        <td><code>${escapeHtml(m.name)}</code></td>
        <td>${humanBytes(m.size)}</td>
        <td style="color:var(--muted);font-size:0.85em;">${exp}</td>
      </tr>`;
    }).join('')}</tbody>
  </table>`;
}

// Pull button — POST /api/ollama/pull {name}; blocks until done (may be minutes).
document.getElementById('llm-pull-btn').onclick = async () => {
  const name = document.getElementById('llm-pull-name').value.trim();
  if (!name) { showToast('Enter a model name.', 'err'); return; }
  const btn = document.getElementById('llm-pull-btn');
  const status = document.getElementById('llm-pull-status');
  btn.disabled = true;
  status.textContent = 'Pulling… (may take a few minutes depending on model size)';
  try {
    const r = await fetch('/api/ollama/pull', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name }),
    });
    if (r.status === 404) {
      status.innerHTML = '<span style="color:var(--muted);">Pull API not supported (404)</span>';
      return;
    }
    if (r.status === 502) {
      status.innerHTML = '<span style="color:var(--err);">Ollama not reachable (502)</span>';
      showToast('Ollama not reachable', 'err');
      return;
    }
    if (!r.ok) {
      const txt = await r.text();
      status.innerHTML = `<span style="color:var(--err);">Error ${r.status}: ${escapeHtml(txt)}</span>`;
      showToast(`pull failed: ${txt}`, 'err');
      return;
    }
    const d = await r.json();
    // Backend returns {status, name}; status === "success" on happy path.
    status.innerHTML = `<span class="badge ok">✓ ${escapeHtml(d.status || 'ok')}</span>`;
    showToast(`✓ ${d.name || name} pull complete`, 'ok');
    pushActivity(`ollama pull · ${d.name || name}`);
    loadLlmModels();
  } catch (e) {
    status.innerHTML = `<span style="color:var(--err);">Error: ${escapeHtml(e.message || String(e))}</span>`;
    showToast(`pull error: ${e.message || e}`, 'err');
  } finally {
    btn.disabled = false;
  }
};

document.getElementById('llm-models-refresh').onclick = () => loadLlmModels();
document.getElementById('llm-ps-refresh').onclick = () => loadLlmPs();

// ── Top-level MODE switch (Project | Tools) ──────────────────────────────────
// Project mode = per-project work; Tools mode = multi-provider / orchestration.
// Each mode owns its own sidebar nav set (<nav class="mode-nav" data-mode="…">).
// MODE_PAGES maps each mode to the page-* ids that live under it, and the default
// page shown when the mode is entered. PAGE_MODE inverts it so navigate() can keep
// the visible sidebar in sync when a page is opened programmatically (e.g.
// loadProjects() jumping to 'settings').
const MODE_PAGES = {
  project: {
    default: 'overview',
    pages: ['overview', 'memory', 'compress', 'command', 'statusline', 'settings', 'templates', 'prompts', 'diagnose', 'security'],
  },
  tools: {
    default: 'llm',
    pages: ['llm', 'limits', 'environment', 'usage', 'route', 'connect'],
  },
};
const PAGE_MODE = {};
Object.entries(MODE_PAGES).forEach(([mode, def]) => def.pages.forEach(p => { PAGE_MODE[p] = mode; }));

const MODE_STORAGE_KEY = 'rtrt.mode';
let CURRENT_MODE = 'project';

function savedMode() {
  const m = localStorage.getItem(MODE_STORAGE_KEY);
  return MODE_PAGES[m] ? m : 'project';
}

// Swap which sidebar nav set is visible + the active top-bar segment. When
// `navigateTo` is true (the default), jump to the mode's default page. Pass
// false to only sync the chrome (used when navigate() detects a cross-mode page).
function setMode(mode, navigateTo = true) {
  if (!MODE_PAGES[mode]) mode = 'project';
  CURRENT_MODE = mode;
  localStorage.setItem(MODE_STORAGE_KEY, mode);
  document.querySelectorAll('aside nav.mode-nav').forEach(nav => {
    nav.hidden = nav.dataset.mode !== mode;
  });
  document.querySelectorAll('#mode-switch .mode-seg').forEach(btn => {
    const on = btn.dataset.mode === mode;
    btn.classList.toggle('active', on);
    btn.setAttribute('aria-selected', on ? 'true' : 'false');
  });
  if (navigateTo) navigate(MODE_PAGES[mode].default);
}

// Keep the mode chrome in sync when a page is shown programmatically.
function syncModeForPage(page) {
  const mode = PAGE_MODE[page];
  if (mode && mode !== CURRENT_MODE) setMode(mode, false);
}

// Where to land when the global ("🌐 Global") scope is selected. Project mode
// lands on Capture/Config (where global defaults are edited); Tools mode stays
// on its own default page so switching the project doesn't yank the user out of
// Tools. api.js calls this via a typeof guard (it loads before app.js).
function globalScopeLandingPage() {
  return CURRENT_MODE === 'tools' ? MODE_PAGES.tools.default : 'settings';
}

document.querySelectorAll('#mode-switch .mode-seg').forEach(btn => {
  btn.onclick = () => setMode(btn.dataset.mode);
});

// Restore the persisted mode BEFORE the initial navigate so the right sidebar
// shows. For the Project mode (the HTML default), the shown page is already
// Overview — only sync the chrome so loadProjects()'s own navigate path stays in
// control. For Tools mode, navigate to its default (Providers) so the visible
// page matches the restored sidebar.
const RESTORED_MODE = savedMode();
setMode(RESTORED_MODE, RESTORED_MODE !== 'project');

// Init
document.getElementById('env-bind').textContent = window.location.host;
syncOverviewWindowButtons();
loadProjects();
startOverviewPolling();
loadTemplates();
loadPrompts();
loadModels();  // populates model <select>s throughout the UI
subscribeStream();
document.getElementById('open-palette').onclick = openPalette;
pushActivity('Dashboard ready. Press ⌘K or Ctrl+K to jump anywhere.');
