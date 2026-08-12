(function installDashboardAuthFetch() {
  const TOKEN_KEY = 'rtrt.dashboard.token';
  const nativeFetch = window.fetch.bind(window);
  const GLOBAL_GET_ROUTES = new Set([
    '/api/projects', '/api/projects/overview', '/api/templates', '/api/prompts',
    '/api/metrics', '/api/budget', '/api/models', '/api/ollama/models',
    '/api/ollama/ps', '/api/security/profiles',
  ]);
  const scopedRequests = new Set();
  let projectGeneration = 0;
  let promptInFlight = null;
  let prompted = false;
  const bootstrapPrefix = '#bootstrap=';
  const hadBootstrapFragment = window.location.hash.startsWith(bootstrapPrefix);

  const bootstrapPromise = (async function exchangeBootstrapFragment() {
    if (!hadBootstrapFragment) return true;
    const credential = window.location.hash.slice(bootstrapPrefix.length);
    // Clear credential before validation, network activity, or any other app code.
    window.history.replaceState(null, '', window.location.pathname + window.location.search);
    if (!/^[A-Za-z0-9_-]{87}$/.test(credential)) return false;
    try {
      const response = await nativeFetch('/api/auth/bootstrap', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ credential }),
        cache: 'no-store',
        credentials: 'omit',
      });
      if (!response.ok) return false;
      const payload = await response.json();
      if (!payload || typeof payload.token !== 'string' || !payload.token) return false;
      sessionStorage.setItem(TOKEN_KEY, payload.token);
      return true;
    } catch (_) {
      return false;
    }
  })();
  // App initialization may do non-fetch work as well. Give app.js one explicit
  // gate so bootstrap always completes before the application starts.
  window.dashboardAuthReady = bootstrapPromise;

  function isDashboardApi(input) {
    const raw = input instanceof Request ? input.url : String(input);
    const url = new URL(raw, window.location.href);
    return url.origin === window.location.origin && (url.pathname === '/api' || url.pathname.startsWith('/api/'));
  }

  function isGlobalApi(method, pathname) {
    if (method !== 'GET') return pathname === '/api/auth/bootstrap';
    return GLOBAL_GET_ROUTES.has(pathname)
      || pathname.startsWith('/api/templates/')
      || pathname.startsWith('/api/prompts/')
      || pathname.startsWith('/api/security/profile/');
  }

  function scopedRequest(input, init) {
    const request = input instanceof Request ? input : null;
    const method = String((init && init.method) || (request && request.method) || 'GET').toUpperCase();
    const url = new URL(request ? request.url : String(input), window.location.href);
    if (isGlobalApi(method, url.pathname)) return { input, init, scoped: false };
    const project = typeof window.dashboardSelectedProject === 'function'
      ? window.dashboardSelectedProject()
      : '';
    if (!project) return { error: new Response('project selection required', { status: 428 }) };
    const asserted = url.searchParams.getAll('project');
    if (asserted.length > 1 || (asserted.length === 1 && asserted[0] !== project)) {
      return { error: new Response('project selector is not canonical', { status: 400 }) };
    }
    const options = { ...(init || {}) };
    const headers = new Headers(request ? request.headers : options.headers);
    headers.set('X-RTRT-Project', project);
    options.headers = headers;
    if (typeof options.body === 'string' && /application\/json/i.test(headers.get('Content-Type') || '')) {
      try {
        const body = JSON.parse(options.body);
        if (body && Object.prototype.hasOwnProperty.call(body, 'project') && body.project !== project) {
          return { error: new Response('body project assertion is not canonical', { status: 400 }) };
        }
      } catch (_) { /* backend owns malformed JSON diagnostics */ }
    }
    const controller = new AbortController();
    const sourceSignal = options.signal || (request && request.signal);
    if (sourceSignal) {
      if (sourceSignal.aborted) controller.abort(sourceSignal.reason);
      else sourceSignal.addEventListener('abort', () => controller.abort(sourceSignal.reason), { once: true });
    }
    options.signal = controller.signal;
    return { input, init: options, scoped: true, controller, generation: projectGeneration };
  }

  function invalidateScopedRequests() {
    projectGeneration += 1;
    scopedRequests.forEach(controller => controller.abort('project changed'));
    scopedRequests.clear();
  }
  window.dashboardInvalidateProjectRequests = invalidateScopedRequests;

  function requestWithToken(input, init, token) {
    const options = { ...(init || {}) };
    const headers = new Headers(options.headers || (input instanceof Request ? input.headers : undefined));
    if (token) headers.set('Authorization', `Bearer ${token}`);
    options.headers = headers;
    return nativeFetch(input instanceof Request ? input.clone() : input, options);
  }

  async function requestTokenOnce() {
    if (promptInFlight) return promptInFlight;
    if (prompted) return null;
    prompted = true;
    promptInFlight = Promise.resolve(window.prompt('Dashboard API token:'))
      .then(value => {
        const token = (value || '').trim();
        if (token) sessionStorage.setItem(TOKEN_KEY, token);
        return token || null;
      })
      .finally(() => { promptInFlight = null; });
    return promptInFlight;
  }

  window.fetch = async function dashboardFetch(input, init) {
    if (!isDashboardApi(input)) return nativeFetch(input, init);
    await bootstrapPromise;
    const scoped = scopedRequest(input, init);
    if (scoped.error) return scoped.error;
    if (scoped.controller) scopedRequests.add(scoped.controller);
    let response;
    try {
      response = await requestWithToken(scoped.input, scoped.init, sessionStorage.getItem(TOKEN_KEY));
    } finally {
      if (scoped.controller) scopedRequests.delete(scoped.controller);
    }
    if (scoped.scoped && scoped.generation !== projectGeneration) throw new DOMException('Stale project response', 'AbortError');
    if (scoped.scoped && (response.status === 400 || response.status === 404)) {
      const detail = await response.clone().text().catch(() => '');
      if (/project selector|project selectors|unknown project/i.test(detail)) {
        if (typeof window.dashboardClearProjectSelection === 'function') window.dashboardClearProjectSelection('Project is unavailable or no longer recognized.');
        return response;
      }
    }
    if (response.status !== 401) return response;
    // A fragment is a one-shot login attempt. Never turn its rejection into an
    // unrelated manual credential prompt; direct visits retain that fallback.
    if (hadBootstrapFragment) {
      const action = document.getElementById('dashboard-token-action');
      if (action) action.textContent = 'Bootstrap rejected';
      return response;
    }
    const token = await requestTokenOnce();
    if (!token) {
      const action = document.getElementById('dashboard-token-action');
      if (action) action.textContent = 'Set API token';
      return response;
    }
    if (scoped.scoped && scoped.generation !== projectGeneration) throw new DOMException('Stale project response', 'AbortError');
    response = await requestWithToken(scoped.input, scoped.init, token); // exactly one retry
    if (scoped.scoped && scoped.generation !== projectGeneration) throw new DOMException('Stale project response', 'AbortError');
    if (response.status === 401) {
      sessionStorage.removeItem(TOKEN_KEY);
      const action = document.getElementById('dashboard-token-action');
      if (action) action.textContent = 'Token rejected — clear';
    }
    return response;
  };

  window.addEventListener('DOMContentLoaded', () => {
    const action = document.getElementById('dashboard-token-action');
    if (!action) return;
    action.onclick = () => {
      sessionStorage.removeItem(TOKEN_KEY);
      prompted = false;
      action.textContent = 'API token cleared';
    };
  });
  window.dashboardApiTestHooks = Object.freeze({ isGlobalApi });
})();

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
// Capture buckets (agent-*, p<n>-*, session-hash names) hidden from the
// selector because they're unambiguously machine-generated and unregistered
// — see rtrt-memory::is_capture_bucket_name. Populated by loadProjects().
let HIDDEN_BUCKETS_CACHE = [];
let HIDDEN_CAPTURE_BUCKETS_COUNT = 0;
let HIDDEN_CAPTURE_BUCKET_ROWS = 0;
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

window.dashboardSelectedProject = currentProject;

function isGlobalScope() {
  return !currentProject();
}

function isGlobalProjectValue(value) {
  return value === GLOBAL_PROJECT_VALUE;
}

function escapeAttr(s) {
  return escapeHtml(s).replace(/"/g, '&quot;');
}

function selectedProject() {
  const slug = currentProject();
  return PROJECTS_CACHE.find(p => p.slug === slug) || null;
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
  const linked = new URL(window.location.href).searchParams.get('project') || '';
  const saved = sessionStorage.getItem('rtrt.project') || '';
  const previous = linked || currentProject() || saved;
  try {
    const r = await fetch('/api/projects');
    if (!r.ok) {
      showToast(await securityErrorMessage(r, 'Failed to load projects'), 'err');
      select.innerHTML = '<option value="">Global overview · projects unavailable</option>';
      select.value = '';
      PROJECTS_CACHE = [];
      HIDDEN_CAPTURE_BUCKETS_COUNT = 0;
      HIDDEN_CAPTURE_BUCKET_ROWS = 0;
      refreshOrphanBuckets();
      syncProjectInputs('');
      refreshProjectScopePage();
      updateProjectSelectorStatus();
      return;
    }
    const data = normalizeProjectsResponse(await r.json());
    PROJECTS_CACHE = data.projects;
    HIDDEN_CAPTURE_BUCKETS_COUNT = data.hidden_capture_buckets;
    HIDDEN_CAPTURE_BUCKET_ROWS = data.hidden_capture_bucket_rows;
    const projectOptions = PROJECTS_CACHE.map(p => {
      const label = p.label || p.name || p.slug;
      const path = p.path || p.memory_root || 'path unavailable';
      const count = Number.isSafeInteger(p.mem_count) ? `${p.mem_count} memories` : 'count unavailable';
      const diagnostic = p.available === false ? ` · unavailable${p.diagnostic ? ` (${p.diagnostic})` : ''}` : '';
      return `<option value="${escapeAttr(p.slug)}"${p.available === false ? ' disabled' : ''}>${escapeHtml(label)} · ${escapeHtml(path)} · ${escapeHtml(p.slug)} · ${count}${escapeHtml(diagnostic)}</option>`;
    }).join('');
    select.innerHTML = `<option value="">Global overview · no project selected</option>${projectOptions}`;
    const desired = PROJECTS_CACHE.find(p => p.slug === previous && p.available !== false);
    select.value = desired ? desired.slug : '';
    syncProjectInputs(select.value);
    if (select.value) sessionStorage.setItem('rtrt.project', select.value);
    else sessionStorage.removeItem('rtrt.project');
    refreshProjectScopePage();
    refreshOrphanBuckets();
    updateProjectSelectorStatus();
    if (data.warning) showToast(data.warning, 'err');
  } catch (e) {
    showToast(`Project load error: ${e.message || e}`, 'err');
    select.innerHTML = '<option value="">Global overview · projects unavailable</option>';
    select.value = '';
    PROJECTS_CACHE = [];
    HIDDEN_CAPTURE_BUCKETS_COUNT = 0;
    HIDDEN_CAPTURE_BUCKET_ROWS = 0;
    refreshOrphanBuckets();
    syncProjectInputs('');
    refreshProjectScopePage();
    updateProjectSelectorStatus();
  }
}

// `/api/projects` historically returned a bare ProjectView array. Normalize
// that legacy shape and the current envelope, but do not accept the distinct
// `/api/memory/projects` `{project,count,latest_ts}` wire format as equivalent.
function normalizeProjectsResponse(data) {
  const rawProjects = Array.isArray(data)
    ? data
    : (data && Array.isArray(data.projects) ? data.projects : []);
  const projects = rawProjects.filter(project =>
    project && typeof project.slug === 'string' && /^[A-Za-z0-9_-]{1,128}$/.test(project.slug)
  );
  const boundedCount = value => Number.isSafeInteger(value) && value >= 0 ? value : 0;
  return {
    projects,
    hidden_capture_buckets: Array.isArray(data) ? 0 : boundedCount(data && data.hidden_capture_buckets),
    hidden_capture_bucket_rows: Array.isArray(data) ? 0 : boundedCount(data && data.hidden_capture_bucket_rows),
    warning: Array.isArray(data) || typeof (data && data.warning) !== 'string'
      ? ''
      : data.warning.slice(0, 320),
  };
}
window.dashboardProjectTestHooks = Object.freeze({ normalizeProjectsResponse });

function updateProjectSelectorStatus() {
  const status = document.getElementById('project-selector-status');
  if (!status) return;
  const available = PROJECTS_CACHE.filter(project => project.available !== false).length;
  const unavailable = PROJECTS_CACHE.length - available;
  const selected = selectedProject();
  status.textContent = selected
    ? `${selected.label || selected.name || selected.slug} · ${selected.mem_count ?? 'unknown'} memories · available`
    : `${available} available · ${unavailable} unavailable · global overview`;
}

function clearProjectSelection(message) {
  if (typeof window.dashboardInvalidateProjectRequests === 'function') window.dashboardInvalidateProjectRequests();
  const select = document.getElementById('project-selector');
  if (select) select.value = '';
  sessionStorage.removeItem('rtrt.project');
  syncProjectInputs('');
  if (typeof resetProjectUiState === 'function') resetProjectUiState();
  if (typeof window.dashboardResetProjectStream === 'function') window.dashboardResetProjectStream();
  updateProjectSelectorStatus();
  if (typeof syncUrlProject === 'function') syncUrlProject();
  if (message) showToast(message, 'err');
}
window.dashboardClearProjectSelection = clearProjectSelection;

/// Populate the "orphaned capture buckets" note + reassign picker inside the
/// project modal from the counts `loadProjects()` just read off `GET
/// /api/projects`. Fetches `GET /api/projects/hidden` for the bucket names
/// only when there's something to show, so the common (zero-orphan) case
/// costs nothing extra.
async function refreshOrphanBuckets() {
  const section = document.getElementById('orphan-buckets-section');
  const note = document.getElementById('orphan-buckets-note');
  const bucketSelect = document.getElementById('orphan-bucket-select');
  const targetSelect = document.getElementById('orphan-target-select');
  if (!section || !note || !bucketSelect || !targetSelect) return;
  if (!HIDDEN_CAPTURE_BUCKETS_COUNT) {
    section.hidden = true;
    HIDDEN_BUCKETS_CACHE = [];
    return;
  }
  const bucketWord = HIDDEN_CAPTURE_BUCKETS_COUNT === 1 ? 'bucket' : 'buckets';
  const rowWord = HIDDEN_CAPTURE_BUCKET_ROWS === 1 ? 'row' : 'rows';
  note.textContent = `${HIDDEN_CAPTURE_BUCKETS_COUNT} orphaned capture ${bucketWord} hidden (${HIDDEN_CAPTURE_BUCKET_ROWS} ${rowWord}) — fold one into a project below.`;
  section.hidden = false;
  try {
    const r = await fetch('/api/projects/hidden');
    const hidden = r.ok ? await r.json() : [];
    HIDDEN_BUCKETS_CACHE = Array.isArray(hidden) ? hidden : [];
  } catch (e) {
    HIDDEN_BUCKETS_CACHE = [];
  }
  bucketSelect.innerHTML = HIDDEN_BUCKETS_CACHE.length
    ? HIDDEN_BUCKETS_CACHE.map(b => `<option value="${escapeAttr(b.name)}">${escapeHtml(b.name)} · ${b.mem_count}</option>`).join('')
    : '<option value="">No hidden buckets</option>';
  const targets = PROJECTS_CACHE.filter(p => p.available !== false);
  targetSelect.innerHTML = targets.length
    ? targets.map(p => `<option value="${escapeAttr(p.slug)}">${escapeHtml(p.label || p.name || p.slug)} · ${escapeHtml(p.slug)}</option>`).join('')
    : '<option value="">No projects to fold into</option>';
}

document.getElementById('orphan-reassign-btn').onclick = async () => {
  const from = document.getElementById('orphan-bucket-select').value;
  const to = document.getElementById('orphan-target-select').value;
  if (!from || !to) { showToast('Pick both an orphan bucket and a target project.', 'err'); return; }
  if (from === to) { showToast('Bucket and target must differ.', 'err'); return; }
  try {
    const r = await fetch('/api/projects/reassign', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ from, to }),
    });
    if (!r.ok) { showToast(await securityErrorMessage(r, 'Failed to fold bucket in'), 'err'); return; }
    const data = await r.json();
    showToast(`Folded ${data.moved} row${data.moved === 1 ? '' : 's'} from ${from} into ${to}`, 'ok');
    await loadProjects();
  } catch (e) {
    showToast(`Reassign error: ${e.message || e}`, 'err');
  }
};

function closeProjectModal() {
  document.getElementById('project-modal').hidden = true;
}

async function openProjectModal(forceNew) {
  showToast('Project registry changes are unavailable in dashboard mode.', 'err');
  return;
  /* istanbul ignore next -- retained markup for compatibility with older servers */
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
  if (typeof window.dashboardInvalidateProjectRequests === 'function') window.dashboardInvalidateProjectRequests();
  if (name) sessionStorage.setItem('rtrt.project', name);
  else sessionStorage.removeItem('rtrt.project');
  if (typeof resetProjectUiState === 'function') resetProjectUiState();
  if (typeof window.dashboardResetProjectStream === 'function') window.dashboardResetProjectStream();
  syncProjectInputs(isGlobalScope() ? '' : name);
  updateGlobalScopeIndicators();
  updateProjectSelectorStatus();
  // Keep the shareable ?project= in the address bar current (replaceState — a
  // project switch is not a new history entry). Defined in app.js.
  if (typeof syncUrlProject === 'function') syncUrlProject();
  refreshProjectScopePage();
  if (isGlobalScope() || activePage() === 'overview') navigate('overview');
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
    sessionStorage.setItem('rtrt.project', name);
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
    // Reflect a direct sub-tab click into the address bar (deep route). These
    // clicks don't go through navigate(), so sync the URL here. Defined in app.js.
    if (typeof syncUrl === 'function') {
      syncUrl(typeof activePage === 'function' ? activePage() : a.dataset.page, { sub: a.dataset.sub });
    }
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
  // Sessions — memories grouped by agent session (GET /api/memory/sessions).
  if (sub === 'memsessions' && project) loadMemSessions(project);
  // Stats tab now also hosts the merged-in Manage section, so load stats +
  // compression queue + the governance summary together when it activates.
  if (sub === 'memstats' && project) { loadMemStats(project); loadQueue(project); loadEmbeddingsProject(); loadGovStats(project); }
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
