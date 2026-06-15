(function initTheme() {
  const saved = localStorage.getItem('rtrt-theme');
  const prefersDark = window.matchMedia && window.matchMedia('(prefers-color-scheme: dark)').matches;
  document.documentElement.setAttribute('data-theme', saved || (prefersDark ? 'dark' : 'light'));
})();
document.getElementById('theme-toggle').onclick = () => {
  const next = (document.documentElement.getAttribute('data-theme') === 'dark') ? 'light' : 'dark';
  document.documentElement.setAttribute('data-theme', next);
  localStorage.setItem('rtrt-theme', next);
};

// Sidebar nav
document.querySelectorAll('aside a.nav').forEach(a => a.onclick = () => {
  navigate(a.dataset.page, {
    source: a,
    sub: a.dataset.sub || null,
    compressEngine: a.dataset.compressEngine || null,
    compressLevel: a.dataset.compressLevel || null,
    focus: a.dataset.focus || null,
  });
});

// Global project selector
let PROJECTS_CACHE = [];
let SECURITY_PROFILES_CACHE = [];
let GLOBAL_DEFAULT_PROFILE = 'ai-default';
const GLOBAL_PROJECT_VALUE = '__global__';
const GLOBAL_SCOPE_MESSAGE = 'Global mode — select an individual project';
// Mirror of the Rust STATUSLINE_SEGMENTS const. `agents` is the orchestration
// segment (labelled "Agents"); `codex` is kept as a backward-compat alias.
const STATUSLINE_SEGMENTS = ['project', 'branch', 'wip', 'sess', 'ctx', 'cache', 'opt', 'model', 'usage', 'agents', 'savings'];
// Human-friendly labels for the segment toggles (key -> label).
const STATUSLINE_SEGMENT_LABELS = { agents: 'Agents' };
const STATUSLINE_TOKENS_HINT = STATUSLINE_SEGMENTS.map(segment => `{${segment}}`).join(', ');
const OUTPUT_OPTIMIZER_MEASUREMENT_NOTE = 'Deterministic compress only — terse-mode injection savings are not measurable';

function currentProject() {
  return document.getElementById('project-selector').value;
}

function isGlobalScope() {
  return document.getElementById('project-selector').value === GLOBAL_PROJECT_VALUE;
}

function isGlobalProjectValue(value) {
  return value === GLOBAL_PROJECT_VALUE;
}

function escapeAttr(s) {
  return escapeHtml(s).replace(/"/g, '&quot;');
}

function selectedProject() {
  const name = currentProject();
  return PROJECTS_CACHE.find(p => p.name === name) || null;
}

function projectPath() {
  const p = selectedProject();
  return p && p.path ? p.path : '';
}

function activePage() {
  const page = document.querySelector('.page:not([hidden])');
  return page ? page.id.replace(/^page-/, '') : 'overview';
}

function syncProjectInputs(value) {
  CURRENT_PROJECT = value || null;
  PROJECT_INPUTS.forEach(id => {
    const el = document.getElementById(id);
    if (el) el.value = value || '';
  });
}

function projectScopeMessage(needsPath) {
  if (isGlobalScope()) return GLOBAL_SCOPE_MESSAGE;
  if (!currentProject()) return 'Select or add a project';
  if (needsPath && !projectPath()) return 'No path set — add one in Edit project';
  return '';
}

function setScopeState(emptyId, cardSelector, needsPath) {
  const empty = document.getElementById(emptyId);
  const card = document.querySelector(cardSelector);
  const msg = projectScopeMessage(needsPath);
  if (empty) {
    empty.textContent = msg || '';
    empty.hidden = !msg;
  }
  if (card) card.hidden = !!msg;
  return !msg;
}

function updateGlobalScopeIndicators() {
  const global = isGlobalScope();
  const settingsTitle = document.getElementById('settings-title');
  const settingsLede = document.getElementById('settings-lede');
  const settingsBadge = document.getElementById('settings-global-badge');
  if (settingsTitle) {
    settingsTitle.childNodes[0].nodeValue = global ? 'Global default settings ' : 'Settings ';
  }
  if (settingsLede) {
    settingsLede.textContent = global
      ? 'Manage global defaults for the security profile, compression, embeddings, and capture.'
      : 'View and save capture and auto-compress settings.';
  }
  if (settingsBadge) settingsBadge.hidden = !global;
  const securityGlobalCard = document.getElementById('security-global-default-card');
  if (securityGlobalCard) securityGlobalCard.hidden = !global;
}

function showGlobalScopeEmpty(targetId) {
  const el = document.getElementById(targetId);
  if (el) el.innerHTML = `<div class="empty">${GLOBAL_SCOPE_MESSAGE}</div>`;
}

async function fetchSecurityProfiles() {
  try {
    const r = await fetch('/api/security/profiles');
    if (!r.ok) {
      showToast(await securityErrorMessage(r, 'Failed to load security profiles'), 'err');
      return [];
    }
    const profiles = await r.json();
    SECURITY_PROFILES_CACHE = Array.isArray(profiles) ? profiles : [];
    return SECURITY_PROFILES_CACHE;
  } catch (e) {
    showToast(`Security profile load error: ${e.message || e}`, 'err');
    return [];
  }
}

async function populateSecurityProfileSelect(selectId, selected) {
  const select = document.getElementById(selectId);
  if (!select) return [];
  const profiles = await fetchSecurityProfiles();
  if (!profiles.length) {
    select.innerHTML = '<option value="">No profiles</option>';
    return [];
  }
  const desired = profiles.includes(selected) ? selected : (profiles.includes(GLOBAL_DEFAULT_PROFILE) ? GLOBAL_DEFAULT_PROFILE : profiles[0]);
  select.innerHTML = profiles.map(name =>
    `<option value="${escapeAttr(name)}"${name === desired ? ' selected' : ''}>${escapeHtml(name)}</option>`
  ).join('');
  select.value = desired;
  return profiles;
}

async function loadProjects() {
  const select = document.getElementById('project-selector');
  const saved = localStorage.getItem('rtrt.project') || localStorage.getItem('rtrt-project') || '';
  const previous = currentProject() || saved || GLOBAL_PROJECT_VALUE;
  const globalOption = `<option value="${GLOBAL_PROJECT_VALUE}">🌐 Global · default</option>`;
  try {
    const r = await fetch('/api/projects');
    if (!r.ok) {
      showToast(await securityErrorMessage(r, 'Failed to load projects'), 'err');
      select.innerHTML = globalOption + '<option value="">Failed to load projects</option>';
      select.value = GLOBAL_PROJECT_VALUE;
      PROJECTS_CACHE = [];
      syncProjectInputs('');
      refreshProjectScopePage();
      navigate('settings');
      return;
    }
    const projects = await r.json();
    PROJECTS_CACHE = Array.isArray(projects) ? projects : [];
    const projectOptions = PROJECTS_CACHE.length ? PROJECTS_CACHE.map(p =>
      `<option value="${escapeAttr(p.name)}">${escapeHtml(p.name)}${p.mem_count ? ` · ${p.mem_count}` : ''}</option>`
    ).join('') : '<option value="">No projects</option>';
    select.innerHTML = globalOption + projectOptions;
    select.value = previous === GLOBAL_PROJECT_VALUE || PROJECTS_CACHE.some(p => p.name === previous) ? previous : GLOBAL_PROJECT_VALUE;
    syncProjectInputs(isGlobalScope() ? '' : select.value);
    localStorage.setItem('rtrt.project', select.value);
    localStorage.setItem('rtrt-project', select.value);
    refreshProjectScopePage();
    if (isGlobalScope()) navigate('settings');
  } catch (e) {
    showToast(`Project load error: ${e.message || e}`, 'err');
    select.innerHTML = globalOption + '<option value="">Failed to load projects</option>';
    select.value = GLOBAL_PROJECT_VALUE;
    PROJECTS_CACHE = [];
    syncProjectInputs('');
    refreshProjectScopePage();
    navigate('settings');
  }
}

function closeProjectModal() {
  document.getElementById('project-modal').hidden = true;
}

async function openProjectModal(forceNew) {
  const project = forceNew ? null : selectedProject();
  document.getElementById('project-name-input').value = project ? project.name : '';
  document.getElementById('project-path-input').value = project && project.path ? project.path : '';
  await populateSecurityProfileSelect('project-security-profile-select', project && project.security_profile ? project.security_profile : GLOBAL_DEFAULT_PROFILE);
  // Per-project embedding override: null/undefined -> Global default, true -> on, false -> off.
  const embSel = document.getElementById('project-embeddings-select');
  if (embSel) {
    const ee = project ? project.embeddings_enabled : null;
    embSel.value = ee === true ? 'on' : ee === false ? 'off' : '';
  }
  document.getElementById('project-modal').hidden = false;
  setTimeout(() => document.getElementById('project-name-input').focus(), 0);
}

document.getElementById('project-add-btn').onclick = () => openProjectModal(false);
document.getElementById('project-modal-close').onclick = closeProjectModal;
document.getElementById('project-modal').onclick = (ev) => { if (ev.target.id === 'project-modal') closeProjectModal(); };
document.getElementById('project-selector').onchange = () => {
  const name = currentProject();
  localStorage.setItem('rtrt.project', name);
  localStorage.setItem('rtrt-project', name);
  syncProjectInputs(isGlobalScope() ? '' : name);
  updateGlobalScopeIndicators();
  if (isGlobalScope()) {
    navigate('settings');
    return;
  }
  refreshProjectScopePage();
  if (activePage() === 'overview') loadOverview();
};
document.getElementById('project-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const name = document.getElementById('project-name-input').value.trim();
  const path = document.getElementById('project-path-input').value.trim();
  const security_profile = document.getElementById('project-security-profile-select').value || GLOBAL_DEFAULT_PROFILE;
  if (!name) { showToast('Enter a project name.', 'err'); return; }
  if (isGlobalProjectValue(name)) { showToast('That name is reserved for the global entry.', 'err'); return; }
  // '' -> Global default(inherit), 'on'/'off' explicit. Sent as a tri-state string.
  const embVal = document.getElementById('project-embeddings-select').value;
  const embeddings_mode = embVal === 'on' ? 'on' : embVal === 'off' ? 'off' : 'inherit';
  try {
    const r = await fetch('/api/projects', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name, path, security_profile, embeddings_mode }),
    });
    if (!r.ok) {
      showToast(await securityErrorMessage(r, 'Failed to save project'), 'err');
      return;
    }
    await r.json().catch(() => ({}));
    closeProjectModal();
    localStorage.setItem('rtrt.project', name);
    document.getElementById('project-selector').value = name;
    await loadProjects();
    document.getElementById('project-selector').value = name;
    syncProjectInputs(name);
    refreshProjectScopePage();
    showToast('Project saved', 'ok');
  } catch (e) {
    showToast(`Project save error: ${e.message || e}`, 'err');
  }
};

// Sub-tabs (tools, settings). Optional onActivate(subName) callback.
function wireSubtabs(navId, onActivate) {
  const nav = document.getElementById(navId);
  if (!nav) return;
  nav.querySelectorAll('a').forEach(a => a.onclick = () => {
    nav.querySelectorAll('a').forEach(x => x.classList.remove('active'));
    a.classList.add('active');
    const parent = nav.parentElement;
    parent.querySelectorAll('.subpage').forEach(x => x.hidden = true);
    document.getElementById('sub-' + a.dataset.sub).hidden = false;
    if (onActivate) onActivate(a.dataset.sub);
  });
}
wireSubtabs('memory-subtabs', (sub) => {
  const project = currentProject();
  // The map (Map) works in global scope when in brain mode (GLOBAL merged brain),
  // so handle it BEFORE the global-scope early-return that other subtabs use.
  if (sub === 'memmap') { loadMemmap(project); return; }
  // Stop the continuous physics sim when leaving the map so it doesn't burn CPU.
  memmapStopLayout();
  if (isGlobalScope()) { refreshMemoryScope(); return; }
  // Auto-load stats + compression queue when the stats tab is activated.
  if (sub === 'memstats' && project) { loadMemStats(project); loadQueue(project); loadEmbeddingsProject(); }
  // Auto-load governance stats when governance tab opens.
  if (sub === 'memgovern' && project) { loadGovStats(project); }
});
wireSubtabs('security-subtabs', (sub) => {
  if (sub === 'securityprofiles') loadSecurityProfileSettings();
  if (sub === 'securityscan') refreshSecurityScope();
});
wireSubtabs('command-subtabs', (sub) => {
  if (sub === 'command-gain') startGainPolling();
  else stopGainPolling();
  if (sub === 'command-coverage') renderCommandCoverage();
  if (sub === 'command-repomap') refreshRepomapScope();
});

// Memory: project drill-in from the global selector
function relativeTime(ts) {
  if (!ts) return '—';
  const diff = Math.floor(Date.now() / 1000 - ts);
  if (diff < 60) return 'just now';
  if (diff < 3600) return `${Math.floor(diff / 60)}m ago`;
  if (diff < 86400) return `${Math.floor(diff / 3600)}h ago`;
  if (diff < 86400 * 7) return `${Math.floor(diff / 86400)}d ago`;
  const d = new Date(ts * 1000);
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
}
