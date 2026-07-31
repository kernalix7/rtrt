// Tools › Orchestration — editor for the `[team]` roster and `[failover]`
// policy exposed by /api/team/config and /api/failover/config.
//
// The roster rtrt ships is only a DEFAULT. Nothing on this page hardcodes a
// lane name, a tier name, a target or a model: lanes and tiers come from the
// config, target/model suggestions come from /api/detect, and the only fixed
// vocabulary is the invocation-mode enum the config schema itself defines.
//
// Classic script, loaded after pages.js and before app.js. Depends on
// escapeHtml / escapeAttr / showToast / pushActivity / applyScopeToggle /
// scopeHasProject / scopeProjectQuery from the earlier files.

// The one fixed vocabulary on this page: `TeamMode` is an enum in the config
// schema, not a user-extensible list, so its three values are the schema's.
const ORCH_MODES = ['cli', 'api', 'auto'];

// Working copy of the config. Edits mutate this; only Save sends it.
let ORCH_TEAM = null;      // team config as returned by GET /api/team/config
let ORCH_FAILOVER = null;  // failover config
let ORCH_TOOLS = [];       // /api/detect rows, for target/model suggestions
// Lane index the last validation error pointed at, so the offending card can be
// highlighted instead of leaving the user to find it.
let ORCH_INVALID_LANE = null;

// ── helpers ─────────────────────────────────────────────────────────────────

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

// Ordered list <-> comma-separated text. Order is meaningful (it is preference
// order), so entries are never sorted or deduplicated here — the server-side
// validator reports a real duplicate far more clearly than a silent drop.
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

function orchLaneNames() {
  return (ORCH_TEAM && ORCH_TEAM.members ? ORCH_TEAM.members : []).map(m => m.name).filter(Boolean);
}

// Tier names offered anywhere in the UI: the configured rungs plus the
// effective ones, so a ladder that is still inherited can still be referenced.
function orchTierNames() {
  if (!ORCH_TEAM) return [];
  const names = [];
  const push = (name) => { if (name && !names.includes(name)) names.push(name); };
  (ORCH_TEAM.tiers || []).forEach(t => push(t.tier));
  ((ORCH_TEAM.effective && ORCH_TEAM.effective.tiers) || []).forEach(t => push(t.tier));
  return names;
}

// Models detection knows about for one target, else every model it knows.
function orchModelsForTarget(target) {
  const tool = ORCH_TOOLS.find(t => t.name === target);
  if (tool && Array.isArray(tool.models) && tool.models.length) return tool.models;
  return orchAllModels();
}

function orchAllModels() {
  const seen = [];
  ORCH_TOOLS.forEach(t => (t.models || []).forEach(m => { if (!seen.includes(m)) seen.push(m); }));
  return seen;
}

function orchFillDatalist(id, values) {
  const el = document.getElementById(id);
  if (!el) return;
  el.innerHTML = values.map(v => `<option value="${escapeAttr(v)}">`).join('');
}

function orchOptions(values, selected, placeholder) {
  const head = placeholder ? `<option value="">${escapeHtml(placeholder)}</option>` : '';
  return head + values.map(v =>
    `<option value="${escapeAttr(v)}"${v === selected ? ' selected' : ''}>${escapeHtml(v)}</option>`
  ).join('');
}

// Reset the validation banner. Called before every save attempt so a stale
// error never lingers next to a roster the user has since fixed.
function orchClearError() {
  ORCH_INVALID_LANE = null;
  const card = document.getElementById('orch-error-card');
  if (card) card.hidden = true;
}

// Surface a server-side validation failure verbatim. The core validator names
// the offending lane as `team.members[<i>]`; when it does, the matching card is
// highlighted so the message is actionable rather than just accurate.
function orchShowError(message) {
  const card = document.getElementById('orch-error-card');
  const text = document.getElementById('orch-error-text');
  if (text) text.textContent = message;
  if (card) {
    card.hidden = false;
    card.scrollIntoView({ behavior: 'smooth', block: 'nearest' });
  }
  const match = /members\[(\d+)\]/.exec(message || '');
  ORCH_INVALID_LANE = match ? Number(match[1]) : null;
  renderOrchLanes();
}

// ── load ────────────────────────────────────────────────────────────────────

async function loadOrchestration() {
  const query = scopeProjectQuery();
  try {
    const [teamRes, failRes, detectRes] = await Promise.all([
      fetch(`/api/team/config${query}`),
      fetch(`/api/failover/config${query}`),
      fetch('/api/detect'),
    ]);
    if (!teamRes.ok) {
      const d = await teamRes.json().catch(() => ({}));
      showToast(`Orchestration load failed: ${d.error || teamRes.status}`, 'err');
      return;
    }
    ORCH_TEAM = await teamRes.json();
    ORCH_FAILOVER = failRes.ok ? await failRes.json() : null;
    ORCH_TOOLS = detectRes.ok ? await detectRes.json() : [];
  } catch (e) {
    showToast(`Orchestration load error: ${e.message || e}`, 'err');
    return;
  }
  orchClearError();
  applyOrchScope(ORCH_TEAM);
  renderOrchestration();
}

// Reuse the shared "Follow global / Custom (this project)" toggle — the same
// helper the other per-project settings use, with no extra machinery here.
//
// Fields stay editable in BOTH scopes, unlike the read-only-when-inherited
// settings cards: Save writes whichever layer the toggle names, so editing
// while following global edits the global roster (which is what it did before a
// project layer existed), and editing while Custom writes this project's
// override. `onLock` is therefore deliberately not passed.
//
// One radiogroup governs both sections on the page, so the card reads Custom
// when the project pins EITHER `[team]` or `[failover]` — each Save writes only
// its own section's override, and "Follow global" clears both.
function applyOrchScope(team) {
  const custom = !!(team && team.custom) || !!(ORCH_FAILOVER && ORCH_FAILOVER.custom);
  applyScopeToggle('team', custom ? 'custom' : 'global', {
    hints: {
      custom: 'Custom: this project carries its own roster. Save writes <repo>/.rtrt/config.toml; the global roster is untouched.',
      global: 'Follow global: this project inherits the global roster. Save here edits that global roster, for every project.',
    },
  });
  // The file hints name the layer the API says it read. They are only applied
  // when a path came back with the payload: the pre-save "Custom" flip has no
  // response yet, and showing the global path next to a Custom card would name
  // a file that save is not going to touch. In that case the generic hint
  // `applyScopeToggle` already wrote is the accurate one.
  if (!team || !team.path) return;
  const hint = document.getElementById('team-config-hint');
  if (hint) hint.textContent = `${team.path} [team]`;
  const failHint = document.getElementById('failover-config-hint');
  if (failHint && ORCH_FAILOVER && ORCH_FAILOVER.path) failHint.textContent = `${ORCH_FAILOVER.path} [failover]`;
}

function renderOrchestration() {
  if (!ORCH_TEAM) return;
  orchFillDatalist('orch-targets', ORCH_TOOLS.map(t => t.name));
  orchFillDatalist('orch-models-all', orchAllModels());
  orchFillDatalist('orch-lane-names', orchLaneNames());
  orchFillDatalist('orch-tier-names', orchTierNames());
  renderOrchManager();
  renderOrchLanes();
  renderOrchTiers();
  renderOrchPolicy();
  renderOrchFailover();
}

// ── manager + leader order ──────────────────────────────────────────────────

function renderOrchManager() {
  const set = (id, value) => { const el = document.getElementById(id); if (el) el.value = value == null ? '' : value; };
  const check = (id, value) => { const el = document.getElementById(id); if (el) el.checked = !!value; };
  check('orch-enabled', ORCH_TEAM.enabled);
  set('orch-manager-provider', ORCH_TEAM.manager_provider);
  set('orch-manager-model', ORCH_TEAM.manager_model);
  set('orch-manager-base-url', ORCH_TEAM.manager_base_url);
  renderOrchLeaders();
}

function renderOrchLeaders() {
  const chips = document.getElementById('orch-leader-chips');
  const order = ORCH_TEAM.leader_order || [];
  if (chips) {
    chips.innerHTML = order.length
      ? order.map((name, i) => `<span class="badge orch-chip">
          <button type="button" class="orch-chip-btn" data-leader-move="${i}" data-dir="-1" title="Move earlier" ${i === 0 ? 'disabled' : ''}>←</button>
          <span class="orch-chip-label">${i + 1}. ${escapeHtml(name)}</span>
          <button type="button" class="orch-chip-btn" data-leader-move="${i}" data-dir="1" title="Move later" ${i === order.length - 1 ? 'disabled' : ''}>→</button>
          <button type="button" class="orch-chip-btn orch-danger" data-leader-remove="${i}" title="Remove">×</button>
        </span>`).join('')
      : '<span class="orch-note">No leaders — the roster cannot be used until at least one lane leads.</span>';
    chips.querySelectorAll('[data-leader-move]').forEach(btn => {
      btn.onclick = () => {
        const i = Number(btn.dataset.leaderMove);
        orchMove(ORCH_TEAM.leader_order, i, Number(btn.dataset.dir));
        renderOrchLeaders();
      };
    });
    chips.querySelectorAll('[data-leader-remove]').forEach(btn => {
      btn.onclick = () => {
        ORCH_TEAM.leader_order.splice(Number(btn.dataset.leaderRemove), 1);
        renderOrchLeaders();
      };
    });
  }
  // Only lanes that are not already leaders can be added.
  const select = document.getElementById('orch-leader-add');
  if (select) {
    const available = orchLaneNames().filter(n => !order.includes(n));
    select.innerHTML = available.length
      ? orchOptions(available, null, null)
      : '<option value="">every lane already leads</option>';
    select.disabled = !available.length;
  }
}

// Swap an element with its neighbour; a no-op at the ends.
function orchMove(list, index, delta) {
  const next = index + delta;
  if (!list || next < 0 || next >= list.length) return;
  const [item] = list.splice(index, 1);
  list.splice(next, 0, item);
}

// ── lanes ───────────────────────────────────────────────────────────────────

function renderOrchLanes() {
  const host = document.getElementById('orch-lanes');
  if (!host || !ORCH_TEAM) return;
  const members = ORCH_TEAM.members || [];
  if (!members.length) {
    host.innerHTML = '<div class="empty">No lanes. Add one to build a roster.</div>';
    return;
  }
  const laneNames = orchLaneNames();
  const chains = (ORCH_TEAM.effective && ORCH_TEAM.effective.chains) || {};

  host.innerHTML = members.map((lane, i) => {
    const others = laneNames.filter(n => n && n !== lane.name);
    const chain = chains[lane.name] || [];
    const flags = Object.entries(lane.flags || {});
    return `<div class="orch-lane${ORCH_INVALID_LANE === i ? ' invalid' : ''}" data-lane="${i}">
      <div class="orch-lane-head">
        <input type="text" class="orch-lane-name" data-lane-field="name" value="${escapeAttr(lane.name || '')}" placeholder="lane name" autocomplete="off">
        ${lane.allow_impl ? '' : '<span class="badge warn">design only</span>'}
        <span class="orch-spacer"></span>
        <button type="button" class="ghost orch-mini" data-lane-move="${i}" data-dir="-1" ${i === 0 ? 'disabled' : ''}>↑</button>
        <button type="button" class="ghost orch-mini" data-lane-move="${i}" data-dir="1" ${i === members.length - 1 ? 'disabled' : ''}>↓</button>
        <button type="button" class="ghost orch-mini orch-danger" data-lane-remove="${i}">Remove</button>
      </div>
      <div class="orch-grid">
        <label class="field-label"><span>target</span>
          <input type="text" data-lane-field="target" list="orch-targets" value="${escapeAttr(lane.target || '')}" autocomplete="off">
        </label>
        <label class="field-label"><span>model</span>
          <input type="text" data-lane-field="model" list="orch-lane-models-${i}" value="${escapeAttr(lane.model || '')}" placeholder="target default" autocomplete="off">
          <datalist id="orch-lane-models-${i}">${orchModelsForTarget(lane.target).map(m => `<option value="${escapeAttr(m)}">`).join('')}</datalist>
        </label>
        <label class="field-label"><span>invocation mode</span>
          <select data-lane-field="mode">${orchOptions(ORCH_MODES, lane.mode, null)}</select>
        </label>
        <label class="field-label"><span>logical model</span>
          <input type="text" data-lane-field="logical" value="${escapeAttr(lane.logical || '')}" placeholder="unset" autocomplete="off">
        </label>
        <label class="field-label"><span>sibling pool</span>
          <select data-lane-field="sibling">${orchOptions(others, lane.sibling || '', 'none')}</select>
        </label>
        <label class="field-label"><span>self-declared tier</span>
          <input type="text" data-lane-field="tier" list="orch-tier-names" value="${escapeAttr(lane.tier || '')}" placeholder="unset" autocomplete="off">
        </label>
        <label class="field-label orch-wide"><span>roles (comma separated)</span>
          <input type="text" data-lane-field="roles" value="${escapeAttr((lane.roles || []).join(', '))}" autocomplete="off">
        </label>
        <label class="field-label orch-wide"><span>fallback lanes (comma separated, most preferred first)</span>
          <input type="text" data-lane-field="fallback" value="${escapeAttr((lane.fallback || []).join(', '))}" autocomplete="off">
        </label>
      </div>
      <div class="orch-row">
        <label class="segment-toggle"><input type="checkbox" data-lane-field="allow_impl" ${lane.allow_impl ? 'checked' : ''}><span>May implement (write code)</span></label>
      </div>
      <div class="settings-section orch-flags-section">
        <h3>Invocation flags</h3>
        <div class="orch-note">Passed through verbatim by whoever invokes this lane. rtrt stores and renders them; it does not interpret them.</div>
        <div class="orch-flags">${flags.map(([key, value], f) => `<div class="orch-flag-row">
          <input type="text" data-flag-key="${f}" value="${escapeAttr(key)}" placeholder="flag" autocomplete="off">
          <input type="text" data-flag-value="${f}" value="${escapeAttr(value)}" placeholder="value (may be empty)" autocomplete="off">
          <button type="button" class="ghost orch-mini orch-danger" data-flag-remove="${f}">×</button>
        </div>`).join('')}</div>
        <button type="button" class="ghost orch-mini" data-flag-add>＋ Add flag</button>
      </div>
      ${chain.length ? `<div class="orch-chain">failure walk: ${escapeHtml(lane.name)} → ${chain.map(escapeHtml).join(' → ')}</div>` : ''}
    </div>`;
  }).join('');

  wireOrchLanes(host);
}

function wireOrchLanes(host) {
  host.querySelectorAll('.orch-lane').forEach(card => {
    const index = Number(card.dataset.lane);
    const lane = ORCH_TEAM.members[index];
    if (!lane) return;

    card.querySelectorAll('[data-lane-field]').forEach(input => {
      const field = input.dataset.laneField;
      // Text edits write straight into the working copy WITHOUT re-rendering,
      // so the caret is never yanked out of the field mid-word. Structural
      // fields (name / target) re-render on `change`, i.e. on blur, once the
      // dependent selects and datalists actually need refreshing.
      if (input.type === 'checkbox') {
        input.onchange = () => { lane[field] = input.checked; renderOrchLanes(); };
        return;
      }
      const commit = () => {
        if (field === 'roles' || field === 'fallback') lane[field] = orchSplitList(input.value);
        else if (field === 'mode') lane[field] = input.value;
        else if (field === 'name' || field === 'target') lane[field] = input.value.trim();
        else lane[field] = orchBlankToNull(input.value);
      };
      input.oninput = commit;
      input.onchange = () => {
        commit();
        if (field === 'name') { renderOrchestration(); return; }
        if (field === 'target' || field === 'mode' || field === 'sibling') renderOrchLanes();
      };
    });

    card.querySelectorAll('[data-flag-key], [data-flag-value]').forEach(input => {
      input.onchange = () => {
        const rows = [...card.querySelectorAll('.orch-flag-row')].map(row => [
          row.querySelector('[data-flag-key]').value.trim(),
          row.querySelector('[data-flag-value]').value,
        ]);
        const next = {};
        rows.forEach(([key, value]) => { if (key) next[key] = value; });
        lane.flags = next;
      };
    });
    card.querySelectorAll('[data-flag-remove]').forEach(btn => {
      btn.onclick = () => {
        const keys = Object.keys(lane.flags || {});
        const key = keys[Number(btn.dataset.flagRemove)];
        if (key != null) delete lane.flags[key];
        renderOrchLanes();
      };
    });
    const addFlag = card.querySelector('[data-flag-add]');
    if (addFlag) {
      addFlag.onclick = () => {
        lane.flags = lane.flags || {};
        // A blank key is dropped server-side, so seed a unique placeholder the
        // user renames rather than an empty row that silently vanishes.
        let n = Object.keys(lane.flags).length + 1;
        while (lane.flags[`flag-${n}`] !== undefined) n += 1;
        lane.flags[`flag-${n}`] = '';
        renderOrchLanes();
      };
    }
  });

  host.querySelectorAll('[data-lane-move]').forEach(btn => {
    btn.onclick = () => {
      orchMove(ORCH_TEAM.members, Number(btn.dataset.laneMove), Number(btn.dataset.dir));
      renderOrchLanes();
    };
  });
  host.querySelectorAll('[data-lane-remove]').forEach(btn => {
    btn.onclick = () => {
      const index = Number(btn.dataset.laneRemove);
      const name = ORCH_TEAM.members[index] ? ORCH_TEAM.members[index].name : '';
      if (!confirm(`Remove lane "${name}"? References to it elsewhere must be cleaned up before saving.`)) return;
      ORCH_TEAM.members.splice(index, 1);
      renderOrchestration();
    };
  });
}

function addOrchLane() {
  // A new lane starts blank apart from the mode the schema defaults to, so
  // nothing about a shipped roster leaks into a hand-built one.
  ORCH_TEAM.members = ORCH_TEAM.members || [];
  let n = ORCH_TEAM.members.length + 1;
  const taken = orchLaneNames();
  while (taken.includes(`lane-${n}`)) n += 1;
  ORCH_TEAM.members.push({
    name: `lane-${n}`,
    target: '',
    model: null,
    mode: ORCH_MODES[0],
    roles: [],
    logical: null,
    sibling: null,
    tier: null,
    fallback: [],
    allow_impl: true,
    flags: {},
  });
  renderOrchestration();
}

// ── tier ladder ─────────────────────────────────────────────────────────────

function renderOrchTiers() {
  const host = document.getElementById('orch-tiers');
  const note = document.getElementById('orch-tiers-note');
  const pin = document.getElementById('orch-pin-tiers');
  if (!host || !ORCH_TEAM) return;
  const tiers = ORCH_TEAM.tiers || [];
  const laneNames = orchLaneNames();

  // An empty configured table is NOT an empty ladder — it means "inherit".
  const inherited = !tiers.length;
  if (note) {
    note.textContent = inherited
      ? 'No ladder configured — the effective ladder below is inherited. Adding a rung here replaces it outright.'
      : 'This ladder replaces the inherited one outright.';
  }
  if (pin) pin.hidden = !inherited || !((ORCH_TEAM.effective && ORCH_TEAM.effective.tiers) || []).length;

  host.innerHTML = tiers.length ? tiers.map((rung, i) => {
    const members = rung.members || [];
    const available = laneNames.filter(n => !members.includes(n));
    return `<div class="orch-tier" data-tier="${i}">
      <div class="orch-lane-head">
        <input type="text" class="orch-lane-name" data-tier-field="tier" value="${escapeAttr(rung.tier || '')}" placeholder="tier name" autocomplete="off">
        <span class="orch-spacer"></span>
        <button type="button" class="ghost orch-mini" data-tier-move="${i}" data-dir="-1" ${i === 0 ? 'disabled' : ''}>↑</button>
        <button type="button" class="ghost orch-mini" data-tier-move="${i}" data-dir="1" ${i === tiers.length - 1 ? 'disabled' : ''}>↓</button>
        <button type="button" class="ghost orch-mini orch-danger" data-tier-remove="${i}">Remove</button>
      </div>
      <div class="orch-chips">${members.length ? members.map((name, m) => `<span class="badge orch-chip">
        <button type="button" class="orch-chip-btn" data-member-move="${m}" data-dir="-1" title="More preferred" ${m === 0 ? 'disabled' : ''}>←</button>
        <span class="orch-chip-label">${m + 1}. ${escapeHtml(name)}</span>
        <button type="button" class="orch-chip-btn" data-member-move="${m}" data-dir="1" title="Less preferred" ${m === members.length - 1 ? 'disabled' : ''}>→</button>
        <button type="button" class="orch-chip-btn orch-danger" data-member-remove="${m}" title="Remove">×</button>
      </span>`).join('') : '<span class="orch-note">No lanes on this rung — a rung must list at least one.</span>'}</div>
      <div class="orch-row">
        <select data-member-add ${available.length ? '' : 'disabled'}>${available.length ? orchOptions(available, null, null) : '<option value="">every lane is on this rung</option>'}</select>
        <button type="button" class="ghost orch-mini" data-member-add-btn ${available.length ? '' : 'disabled'}>Add lane</button>
      </div>
    </div>`;
  }).join('') : '';

  host.querySelectorAll('.orch-tier').forEach(card => {
    const index = Number(card.dataset.tier);
    const rung = ORCH_TEAM.tiers[index];
    if (!rung) return;
    const nameInput = card.querySelector('[data-tier-field="tier"]');
    if (nameInput) {
      nameInput.oninput = () => { rung.tier = nameInput.value; };
      nameInput.onchange = () => { rung.tier = nameInput.value.trim(); renderOrchestration(); };
    }
    card.querySelectorAll('[data-member-move]').forEach(btn => {
      btn.onclick = () => { orchMove(rung.members, Number(btn.dataset.memberMove), Number(btn.dataset.dir)); renderOrchTiers(); };
    });
    card.querySelectorAll('[data-member-remove]').forEach(btn => {
      btn.onclick = () => { rung.members.splice(Number(btn.dataset.memberRemove), 1); renderOrchTiers(); };
    });
    const addBtn = card.querySelector('[data-member-add-btn]');
    const addSel = card.querySelector('[data-member-add]');
    if (addBtn && addSel) {
      addBtn.onclick = () => {
        const name = addSel.value;
        if (!name) return;
        rung.members = rung.members || [];
        rung.members.push(name);
        renderOrchTiers();
      };
    }
  });

  host.querySelectorAll('[data-tier-move]').forEach(btn => {
    btn.onclick = () => { orchMove(ORCH_TEAM.tiers, Number(btn.dataset.tierMove), Number(btn.dataset.dir)); renderOrchTiers(); };
  });
  host.querySelectorAll('[data-tier-remove]').forEach(btn => {
    btn.onclick = () => { ORCH_TEAM.tiers.splice(Number(btn.dataset.tierRemove), 1); renderOrchestration(); };
  });

  renderOrchEffectiveTiers();
}

function renderOrchEffectiveTiers() {
  const host = document.getElementById('orch-effective-tiers');
  if (!host) return;
  const tiers = (ORCH_TEAM.effective && ORCH_TEAM.effective.tiers) || [];
  host.innerHTML = tiers.length
    ? tiers.map(t => `<span class="badge${t.design_only ? ' warn' : ''}" title="${escapeAttr((t.members || []).join(' → '))}">
        ${escapeHtml(t.tier)} · ${(t.members || []).length} lane${(t.members || []).length === 1 ? '' : 's'}${t.design_only ? ' · design only' : ''}
      </span>`).join('')
    : '<span class="orch-note">No rungs resolve — add a tier, or give a lane its own tier.</span>';
}

function addOrchTier() {
  const input = document.getElementById('orch-tier-name');
  const name = input ? input.value.trim() : '';
  if (!name) { showToast('Name the tier first.', 'err'); return; }
  ORCH_TEAM.tiers = ORCH_TEAM.tiers || [];
  if (ORCH_TEAM.tiers.some(t => t.tier === name)) { showToast('That tier already exists.', 'err'); return; }
  ORCH_TEAM.tiers.push({ tier: name, members: [] });
  if (input) input.value = '';
  renderOrchestration();
}

// Copy the resolved ladder into the configured one, so customising an inherited
// ladder starts from what is actually in force instead of a blank table.
function pinOrchEffectiveTiers() {
  const tiers = (ORCH_TEAM.effective && ORCH_TEAM.effective.tiers) || [];
  ORCH_TEAM.tiers = tiers.map(t => ({ tier: t.tier, members: (t.members || []).slice() }));
  renderOrchestration();
}

// ── policy ──────────────────────────────────────────────────────────────────

function renderOrchPolicy() {
  const policy = ORCH_TEAM.policy || {};
  const effective = ORCH_TEAM.effective || {};
  const set = (id, value) => { const el = document.getElementById(id); if (el) el.value = value == null ? '' : value; };
  const check = (id, value) => { const el = document.getElementById(id); if (el) el.checked = !!value; };
  set('orch-max-retries', policy.max_retries);
  set('orch-max-depth', policy.max_fallback_depth);
  check('orch-redo', policy.redo_on_fallback);
  check('orch-sibling', policy.prefer_sibling_on_quota);
  check('orch-provenance', policy.record_provenance);

  // The depth placeholder shows the value rtrt would derive, so "auto" is a
  // number the user can see rather than a mystery.
  const depth = document.getElementById('orch-max-depth');
  if (depth && effective.max_fallback_depth != null) {
    depth.placeholder = `auto (${effective.max_fallback_depth})`;
  }

  const tierNames = orchTierNames();
  const defaultSel = document.getElementById('orch-default-tier');
  if (defaultSel) {
    const auto = effective.default_tier ? `auto (${effective.default_tier})` : 'auto (first rung)';
    defaultSel.innerHTML = orchOptions(tierNames, policy.default_tier || '', auto);
    defaultSel.value = policy.default_tier || '';
  }

  // Design-only tiers: `null` follows the shipped default, an explicit list
  // pins it. The checkbox distinguishes the two; while following the default
  // the boxes show the effective set read-only.
  const following = policy.design_only_tiers == null;
  check('orch-design-default', following);
  const selected = following
    ? (effective.design_only_tiers || [])
    : policy.design_only_tiers;
  const host = document.getElementById('orch-design-tiers');
  if (host) {
    host.innerHTML = tierNames.length
      ? tierNames.map(name => `<label class="segment-toggle">
          <input type="checkbox" data-design-tier="${escapeAttr(name)}" ${selected.includes(name) ? 'checked' : ''} ${following ? 'disabled' : ''}>
          <span>${escapeHtml(name)}</span>
        </label>`).join('')
      : '<span class="orch-note">No tiers to choose from yet.</span>';
    host.querySelectorAll('[data-design-tier]').forEach(cb => {
      cb.onchange = () => {
        const chosen = [...host.querySelectorAll('[data-design-tier]')]
          .filter(x => x.checked)
          .map(x => x.dataset.designTier);
        ORCH_TEAM.policy.design_only_tiers = chosen;
      };
    });
  }
}

// ── failover ────────────────────────────────────────────────────────────────

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
    const r = await fetch(`/api/failover/config${scopeProjectQuery()}`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    });
    const d = await r.json().catch(() => ({}));
    if (!r.ok) throw new Error(d.error || `${r.status}`);
    ORCH_FAILOVER = d;
    renderOrchFailover();
    // A project-scoped save has just created this project's `[failover]`
    // override, so the shared scope toggle has to catch up.
    applyOrchScope(ORCH_TEAM);
    if (result) result.innerHTML = '<span class="badge ok">✓ Saved</span>';
    pushActivity('Orchestration: failure policy saved');
    showToast('Failure policy saved', 'ok');
  } catch (e) {
    if (result) result.innerHTML = `<span style="color:var(--err);">${escapeHtml(e.message || String(e))}</span>`;
    showToast(`Failure policy save error: ${e.message || e}`, 'err');
  }
}

// ── save the roster ─────────────────────────────────────────────────────────

// The request body: the full desired roster (a full-replace write, so removing
// a lane or a rung is just omitting it).
function orchTeamBody() {
  const val = (id) => { const el = document.getElementById(id); return el ? el.value : ''; };
  const checked = (id) => { const el = document.getElementById(id); return !!(el && el.checked); };
  const followDesignDefault = checked('orch-design-default');
  const policy = ORCH_TEAM.policy || {};
  return {
    enabled: checked('orch-enabled'),
    manager_provider: val('orch-manager-provider'),
    manager_model: val('orch-manager-model'),
    manager_base_url: val('orch-manager-base-url'),
    leader_order: ORCH_TEAM.leader_order || [],
    members: (ORCH_TEAM.members || []).map(lane => ({
      name: lane.name || '',
      target: lane.target || '',
      model: orchBlankToNull(lane.model),
      mode: ORCH_MODES.includes(lane.mode) ? lane.mode : ORCH_MODES[0],
      roles: lane.roles || [],
      logical: orchBlankToNull(lane.logical),
      sibling: orchBlankToNull(lane.sibling),
      tier: orchBlankToNull(lane.tier),
      fallback: lane.fallback || [],
      allow_impl: lane.allow_impl !== false,
      flags: lane.flags || {},
    })),
    tiers: (ORCH_TEAM.tiers || []).map(rung => ({
      tier: rung.tier || '',
      members: rung.members || [],
    })),
    policy: {
      max_retries: orchNumOrNull(val('orch-max-retries')) ?? policy.max_retries ?? 0,
      redo_on_fallback: checked('orch-redo'),
      prefer_sibling_on_quota: checked('orch-sibling'),
      record_provenance: checked('orch-provenance'),
      max_fallback_depth: orchNumOrNull(val('orch-max-depth')),
      default_tier: orchBlankToNull(val('orch-default-tier')),
      // `null` = follow the shipped default; a list pins it explicitly.
      design_only_tiers: followDesignDefault ? null : (policy.design_only_tiers || []),
    },
  };
}

async function saveOrchestration() {
  if (!ORCH_TEAM) return;
  const result = document.getElementById('orch-save-result');
  const btn = document.getElementById('orch-save-btn');
  orchClearError();
  if (result) result.textContent = 'Saving…';
  if (btn) btn.disabled = true;
  try {
    const r = await fetch(`/api/team/config${scopeProjectQuery()}`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(orchTeamBody()),
    });
    const d = await r.json().catch(() => ({}));
    if (!r.ok) {
      // The roster was REJECTED — nothing was written. Keep the working copy
      // as it is so the user can fix the problem instead of losing the edit.
      if (result) result.innerHTML = '<span class="badge err">not saved</span>';
      orchShowError(d.error || `Save failed (${r.status})`);
      showToast('Roster rejected — see the error above', 'err');
      return;
    }
    ORCH_TEAM = d;
    applyOrchScope(ORCH_TEAM);
    renderOrchestration();
    if (result) result.innerHTML = '<span class="badge ok">✓ Saved</span>';
    pushActivity('Orchestration: roster saved');
    showToast('Orchestration saved', 'ok');
  } catch (e) {
    if (result) result.innerHTML = `<span style="color:var(--err);">${escapeHtml(e.message || String(e))}</span>`;
    orchShowError(e.message || String(e));
  } finally {
    if (btn) btn.disabled = false;
  }
}

// ── wiring ──────────────────────────────────────────────────────────────────

(function wireOrchestrationPage() {
  const on = (id, handler, event = 'click') => {
    const el = document.getElementById(id);
    if (el) el.addEventListener(event, handler);
  };
  on('orch-add-lane', addOrchLane);
  on('orch-add-tier', addOrchTier);
  on('orch-pin-tiers', pinOrchEffectiveTiers);
  on('orch-save-btn', saveOrchestration);
  on('orch-reload-btn', () => loadOrchestration());
  on('orch-fail-save-btn', saveOrchFailover);
  on('orch-leader-add-btn', () => {
    const select = document.getElementById('orch-leader-add');
    const name = select ? select.value : '';
    if (!name) return;
    ORCH_TEAM.leader_order = ORCH_TEAM.leader_order || [];
    ORCH_TEAM.leader_order.push(name);
    renderOrchLeaders();
  });
  on('orch-tier-name', (ev) => { if (ev.key === 'Enter') addOrchTier(); }, 'keydown');
  // The design-only "follow the shipped default" switch flips the policy field
  // between `null` (inherit) and an explicit list seeded from what is in force.
  on('orch-design-default', () => {
    const following = document.getElementById('orch-design-default').checked;
    if (!ORCH_TEAM) return;
    ORCH_TEAM.policy = ORCH_TEAM.policy || {};
    ORCH_TEAM.policy.design_only_tiers = following
      ? null
      : (((ORCH_TEAM.effective || {}).design_only_tiers) || []).slice();
    renderOrchPolicy();
  }, 'change');
  // Scope radios. Rendered by the shared `applyScopeToggle`; wired exactly like
  // the other form-and-Save settings cards (providers, agents): "Follow global"
  // clears this project's override immediately, "Custom" flips the card and
  // waits for Save to write it. The one page-specific part is that the single
  // "Orchestration scope" radiogroup governs BOTH sections on the page, so the
  // clear covers `[team]` and `[failover]` together.
  on('team-scope-global', async (ev) => {
    if (!ev.target.checked || !scopeHasProject()) return;
    try {
      const responses = await Promise.all(
        ['/api/team/config', '/api/failover/config'].map(base =>
          fetch(scopeClearUrl(base), { method: 'POST' })
        )
      );
      for (const r of responses) {
        if (!r.ok) {
          const d = await r.json().catch(() => ({}));
          throw new Error(d.error || `${r.status}`);
        }
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
    const result = document.getElementById('orch-save-result');
    if (result) result.textContent = 'Editing project override — click Save to apply.';
    const failResult = document.getElementById('orch-fail-save-result');
    if (failResult) failResult.textContent = 'Editing project override — click Save to apply.';
  }, 'change');
})();
