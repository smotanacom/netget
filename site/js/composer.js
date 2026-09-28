// The "you are the model" composer: answer a model request by picking actions and filling
// in their parameters, instead of hand-writing the JSON envelope.
//
// NetGet sends every offered action with the request (`req.actions`, built by
// `netget::llm::bridge::offered_action`): name, description, parameters with their declared
// types, a JSON Schema, and the action's own example — which its executor accepts. Every
// field starts from that example, so the common case is pick → tweak → send.
//
// It is modelled on the dashboard's intercept composer (src/tui/modal/composer.rs). The
// reply it builds is the one the plain textarea produced: `{content: '{"actions":[...]}'}`,
// with tools under `"tools"`, or native `tool_calls` for a chat request that carries tool
// schemas. Raw JSON stays one tab away and is sent verbatim.
//
// The top half of this file is pure (no DOM) so web/test/smoke.mjs can build the same reply
// under Node and check that NetGet accepts it.

// ---------------------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------------------

/** The actions a request offers; empty when the request (or an older bundle) has none. */
export function offeredActions(req) {
    return Array.isArray(req && req.actions) ? req.actions.filter((a) => a && a.name) : [];
}

/** Which action to preselect: the first one specific to this protocol and event. */
export function defaultActionIndex(actions) {
    const i = actions.findIndex((a) => !a.generic && !a.tool);
    return i >= 0 ? i : 0;
}

const NUMBER_HINT = /^(number|integer|int|float|double|u8|u16|u32|u64|usize|i8|i16|i32|i64|isize)$/i;

/**
 * How a parameter is edited: 'choice' | 'bool' | 'number' | 'json' | 'text'.
 * The example's value decides when there is one (it is known to be accepted); otherwise
 * the declared type hint does.
 */
export function fieldKind(param, exampleValue) {
    if (Array.isArray(param.choices) && param.choices.length) return 'choice';
    if (exampleValue !== undefined && exampleValue !== null) {
        if (typeof exampleValue === 'boolean') return 'bool';
        if (typeof exampleValue === 'number') return 'number';
        if (typeof exampleValue === 'object') return 'json';
        return 'text';
    }
    const hint = String(param.type || '').trim();
    if (/bool/i.test(hint)) return 'bool';
    if (NUMBER_HINT.test(hint)) return 'number';
    if (/array|object|map|list|\[|\{/i.test(hint)) return 'json';
    return 'text';
}

function exampleOf(action) {
    return action && action.example && typeof action.example === 'object' && !Array.isArray(action.example)
        ? action.example : {};
}

/**
 * One action being composed. `fields` holds the raw editor value of each parameter:
 * text → string, number → string, bool → true/false/null (null = not sent),
 * choice → string ('' = not sent), json → string. `extras` are keys the example carries
 * that no parameter declares; they are sent as the example has them.
 */
export function newEntry(action, source) {
    const values = source && typeof source === 'object' ? source : exampleOf(action);
    const declared = new Set((action.parameters || []).map((p) => p.name));
    const fields = (action.parameters || []).map((param) => {
        const v = values[param.name];
        const kind = fieldKind(param, v === undefined ? exampleOf(action)[param.name] : v);
        let raw;
        if (kind === 'bool') raw = v === undefined ? (param.required ? false : null) : v === true || v === 'true';
        else if (kind === 'choice') raw = v === undefined ? (param.required ? param.choices[0] : '') : String(v);
        else if (kind === 'json') raw = v === undefined ? '' : JSON.stringify(v, null, 2);
        else if (kind === 'number') raw = v === undefined ? '' : String(v);
        else raw = v === undefined ? '' : (typeof v === 'string' ? v : JSON.stringify(v));
        // A string that carries CRLF is edited in a textarea, which normalises line breaks to
        // LF; remember to put them back.
        const crlf = kind === 'text' && typeof v === 'string' && v.includes('\r\n');
        return { name: param.name, kind, raw, crlf, required: !!param.required };
    });
    const extras = {};
    for (const [k, v] of Object.entries(values)) {
        if (k !== 'type' && !declared.has(k)) extras[k] = v;
    }
    return { name: action.name, fields, extras };
}

/**
 * The action object an entry stands for: `{ok: true, value}` or `{ok: false, errors}`,
 * where each error is `{field, message}`.
 */
export function buildAction(entry) {
    const value = { type: entry.name };
    const errors = [];
    for (const f of entry.fields) {
        switch (f.kind) {
        case 'bool':
            if (f.raw !== null) value[f.name] = !!f.raw;
            break;
        case 'choice':
            if (f.raw !== '') value[f.name] = f.raw;
            break;
        case 'number': {
            const s = String(f.raw).trim();
            if (s === '') { if (f.required) errors.push({ field: f.name, message: 'required' }); break; }
            const n = Number(s);
            if (!Number.isFinite(n)) errors.push({ field: f.name, message: 'not a number' });
            else value[f.name] = n;
            break;
        }
        case 'json': {
            const s = String(f.raw).trim();
            if (s === '') { if (f.required) errors.push({ field: f.name, message: 'required' }); break; }
            try { value[f.name] = JSON.parse(s); } catch (e) { errors.push({ field: f.name, message: 'not valid JSON: ' + e.message }); }
            break;
        }
        default: {
            if (f.raw === '' && !f.required) break;
            value[f.name] = f.crlf ? String(f.raw).replace(/\r\n/g, '\n').replace(/\n/g, '\r\n') : f.raw;
        }
        }
    }
    Object.assign(value, entry.extras);
    return errors.length ? { ok: false, errors } : { ok: true, value };
}

/** `{actions:[...]}` plus `tools:[...]` when any entry is a tool. */
export function buildEnvelope(actions, entries) {
    const envelope = { actions: [] };
    const tools = [];
    for (const entry of entries) {
        const built = buildAction(entry);
        if (!built.ok) return { ok: false, entry, errors: built.errors };
        const action = actions.find((a) => a.name === entry.name);
        (action && action.tool ? tools : envelope.actions).push(built.value);
    }
    if (tools.length) envelope.tools = tools;
    return { ok: true, envelope };
}

/**
 * The reply NetGet's bridge takes for these entries. A chat request that carries native tool
 * schemas is answered with `tool_calls` when every entry is one of those tools; everything
 * else, and an empty answer, is the JSON envelope as `content`.
 */
export function buildReply(req, actions, entries) {
    const built = buildEnvelope(actions, entries);
    if (!built.ok) return built;
    const toolNames = new Set((req.tools || []).map((t) => (t.function && t.function.name) || t.name));
    if (req.kind === 'chat' && entries.length && entries.every((e) => toolNames.has(e.name))) {
        const calls = [...(built.envelope.tools || []), ...built.envelope.actions].map((v) => {
            const { type, ...args } = v;
            return { name: type, arguments: args };
        });
        return { ok: true, reply: { content: null, tool_calls: calls } };
    }
    return { ok: true, reply: { content: JSON.stringify(built.envelope) } };
}

/** Entries for a parsed envelope, or null if the form cannot show it faithfully. */
export function entriesFromEnvelope(actions, parsed) {
    if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) return null;
    const known = new Set(Object.keys(parsed));
    known.delete('actions'); known.delete('tools');
    if (known.size) return null;
    if (!Array.isArray(parsed.actions || []) || !Array.isArray(parsed.tools || [])) return null;
    const items = [...(parsed.tools || []), ...(parsed.actions || [])];
    const entries = [];
    for (const item of items) {
        const action = item && actions.find((a) => a.name === item.type);
        if (!action) return null;
        entries.push(newEntry(action, item));
    }
    return entries;
}

// ---------------------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------------------

function esc(s) {
    return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
}

function oneLine(text, max = 70) {
    const first = String(text || '').split(/\n|(?<=\.)\s/)[0].trim();
    return first.length > max ? first.slice(0, max - 1) + '…' : first;
}

let uid = 0;

/**
 * Render the composer into `root` and resolve through `onSend(reply)` / `onRefuse()`.
 * Falls back to a plain JSON editor when the request offers no actions. `autofocus: false`
 * leaves the focus where it is (the page keeps it in its Telnet terminal); focusing never
 * scrolls the page.
 */
export function mountComposer(root, req, { onSend, onRefuse, autofocus = true }) {
    const actions = offeredActions(req);
    const id = 'cmp' + (++uid);
    const state = {
        entries: actions.length ? [newEntry(actions[defaultActionIndex(actions)])] : [],
        tab: actions.length ? 'form' : 'raw',
        raw: '',
    };

    root.innerHTML = `
      <div class="llm-reply-label">Your reply</div>
      ${actions.length ? `
      <div class="cmp-tabs" role="tablist" aria-label="How to write the reply">
        <button type="button" role="tab" id="${id}-tab-form" aria-controls="${id}-form" data-tab="form">Form</button>
        <button type="button" role="tab" id="${id}-tab-raw" aria-controls="${id}-raw" data-tab="raw">Raw JSON</button>
      </div>` : `
      <p class="llm-hint">This request offers no actions to pick from. Reply the way the prompt above asks — usually a JSON object with an "actions" array.</p>`}
      <div class="cmp-form" id="${id}-form" role="tabpanel" aria-labelledby="${id}-tab-form"></div>
      <div class="cmp-raw" id="${id}-raw" role="tabpanel" aria-labelledby="${id}-tab-raw">
        <textarea class="llm-input cmp-raw-input" rows="8" spellcheck="false" aria-label="Reply as raw JSON" aria-describedby="${id}-raw-status"></textarea>
        <div class="cmp-raw-status" id="${id}-raw-status" aria-live="polite"></div>
      </div>
      <div class="cmp-error" role="alert"></div>
      <div class="llm-actions">
        <button type="button" class="btn btn-primary llm-send">Send reply</button>
        <button type="button" class="btn cmp-nothing" title='Reply {"actions": []}: a real answer that does nothing, not a failure'>Answer with nothing</button>
        <button type="button" class="btn llm-fail">Refuse (fail closed)</button>
      </div>
    `;

    const formEl = root.querySelector('.cmp-form');
    const rawEl = root.querySelector('.cmp-raw');
    const rawInput = root.querySelector('.cmp-raw-input');
    const rawStatus = root.querySelector('.cmp-raw-status');
    const errorEl = root.querySelector('.cmp-error');

    function currentEnvelopeText() {
        const built = buildEnvelope(actions, state.entries);
        return built.ok ? JSON.stringify(built.envelope, null, 2) : null;
    }

    // The raw editor grows with its text instead of scrolling.
    function fitRaw() {
        if (rawEl.hidden) return;
        rawInput.style.height = 'auto';
        rawInput.style.height = rawInput.scrollHeight + 2 + 'px';
    }

    function validateRaw() {
        fitRaw();
        const text = rawInput.value;
        if (!text.trim()) {
            rawInput.removeAttribute('aria-invalid');
            rawStatus.textContent = '';
            return null;
        }
        let parsed;
        try { parsed = JSON.parse(text); } catch (e) {
            rawInput.setAttribute('aria-invalid', 'true');
            rawStatus.textContent = 'Not valid JSON (' + e.message + '). It will be sent as typed.';
            rawStatus.className = 'cmp-raw-status is-bad';
            return null;
        }
        rawInput.removeAttribute('aria-invalid');
        const entries = actions.length ? entriesFromEnvelope(actions, parsed) : null;
        rawStatus.className = 'cmp-raw-status is-ok';
        rawStatus.textContent = entries
            ? 'Valid JSON. The form shows the same answer.'
            : 'Valid JSON' + (actions.length ? ', but the form cannot show it (an unknown action or key); it will be sent as typed.' : '.');
        return entries;
    }

    function showTab(tab) {
        errorEl.textContent = '';
        if (tab === 'raw') {
            const text = currentEnvelopeText();
            if (text !== null) rawInput.value = text;
            else if (!rawInput.value) rawInput.value = '{"actions": []}';
            validateRaw();
        } else {
            const entries = validateRaw();
            if (entries) state.entries = entries;
            renderForm();
        }
        state.tab = tab;
        formEl.hidden = tab !== 'form';
        rawEl.hidden = tab !== 'raw';
        fitRaw();
        for (const b of root.querySelectorAll('[role=tab]')) {
            const on = b.dataset.tab === tab;
            b.setAttribute('aria-selected', on ? 'true' : 'false');
            b.tabIndex = on ? 0 : -1;
            b.classList.toggle('is-active', on);
        }
    }

    function renderField(entry, ei, f, fi, action) {
        const param = action.parameters[fi];
        const fid = `${id}-e${ei}-f${fi}`;
        const descId = fid + '-d';
        const req = f.required ? '<span class="cmp-req" aria-hidden="true">*</span>' : '';
        const common = `id="${fid}" data-ei="${ei}" data-fi="${fi}" aria-describedby="${descId}"${f.required ? ' aria-required="true"' : ''}`;
        let control;
        switch (f.kind) {
        case 'bool':
            control = `<input type="checkbox" ${common}${f.raw ? ' checked' : ''}>`;
            break;
        case 'choice': {
            const choices = f.raw !== '' && !param.choices.includes(f.raw) ? [...param.choices, f.raw] : param.choices;
            control = `<select ${common}>${f.required ? '' : `<option value=""${f.raw === '' ? ' selected' : ''}>(not sent)</option>`}${choices.map((c) => `<option${c === f.raw ? ' selected' : ''}>${esc(c)}</option>`).join('')}</select>`;
            break;
        }
        case 'number':
            control = `<input type="number" step="any" ${common} value="${esc(f.raw)}">`;
            break;
        case 'json':
            control = `<textarea rows="${Math.min(8, Math.max(2, String(f.raw).split('\n').length))}" spellcheck="false" class="cmp-json" ${common}>${esc(f.raw)}</textarea>`;
            break;
        default:
            control = /[\r\n]/.test(f.raw) || String(f.raw).length > 60
                ? `<textarea rows="${Math.min(6, Math.max(2, String(f.raw).split('\n').length))}" spellcheck="false" ${common}>${esc(f.raw)}</textarea>`
                : `<input type="text" spellcheck="false" ${common} value="${esc(f.raw)}">`;
        }
        const crlf = f.kind === 'text' && /[\r\n]/.test(f.raw)
            ? `<label class="cmp-crlf"><input type="checkbox" data-crlf data-ei="${ei}" data-fi="${fi}"${f.crlf ? ' checked' : ''}> send line breaks as CRLF</label>` : '';
        const unset = f.kind === 'bool' && !f.required
            ? `<button type="button" class="cmp-unset" data-unset data-ei="${ei}" data-fi="${fi}"${f.raw === null ? ' hidden' : ''}>don't send</button>` : '';
        const notSent = f.kind === 'bool' && f.raw === null ? ' <em class="cmp-dim">(not sent)</em>' : '';
        return `
          <div class="cmp-field cmp-kind-${f.kind}">
            <label for="${fid}"><code>${esc(f.name)}</code>${req} <span class="cmp-type">${esc(param.type)}</span>${notSent}</label>
            <div class="cmp-control">${control}${unset}${crlf}</div>
            <div class="cmp-desc" id="${descId}">${esc(param.description || '')}<span class="cmp-field-error"></span></div>
          </div>`;
    }

    function renderForm() {
        if (!actions.length) { formEl.innerHTML = ''; return; }
        formEl.innerHTML = state.entries.map((entry, ei) => {
            const action = actions.find((a) => a.name === entry.name);
            const extras = Object.keys(entry.extras);
            return `
              <fieldset class="cmp-entry" data-ei="${ei}">
                <legend>Action ${ei + 1}</legend>
                <div class="cmp-pick">
                  <label for="${id}-e${ei}-pick" class="cmp-sr">Action ${ei + 1}</label>
                  <select id="${id}-e${ei}-pick" class="cmp-picker" data-ei="${ei}" aria-describedby="${id}-e${ei}-about">
                    ${actions.map((a) => `<option value="${esc(a.name)}"${a.name === entry.name ? ' selected' : ''}>${esc(a.name)}${a.tool ? ' (tool)' : ''} — ${esc(oneLine(a.description))}</option>`).join('')}
                  </select>
                  ${state.entries.length > 1 ? `<button type="button" class="btn cmp-remove" data-ei="${ei}" aria-label="Remove action ${ei + 1}">Remove</button>` : ''}
                </div>
                <p class="cmp-about" id="${id}-e${ei}-about">${esc(action.description || '')}</p>
                ${entry.fields.length ? entry.fields.map((f, fi) => renderField(entry, ei, f, fi, action)).join('') : '<p class="cmp-dim">No parameters.</p>'}
                ${extras.length ? `<p class="cmp-dim">Also sends ${extras.map((k) => `<code>${esc(k)}</code>`).join(', ')} from the example (edit under Raw JSON).</p>` : ''}
                <details class="cmp-schema">
                  <summary>Schema and example</summary>
                  <pre>${esc(JSON.stringify(action.schema ?? {}, null, 2))}</pre>
                  <pre>${esc(JSON.stringify(action.example ?? {}, null, 2))}</pre>
                </details>
              </fieldset>`;
        }).join('') + '<button type="button" class="btn cmp-add">+ Add another action</button>';
    }

    function fieldAt(el) {
        const entry = state.entries[+el.dataset.ei];
        return entry && entry.fields[+el.dataset.fi];
    }

    function showFieldError(ei, fi, message) {
        const input = formEl.querySelector(`[data-ei="${ei}"][data-fi="${fi}"]:not([data-crlf]):not([data-unset])`);
        if (!input) return;
        input.setAttribute('aria-invalid', message ? 'true' : 'false');
        const slot = input.closest('.cmp-field').querySelector('.cmp-field-error');
        slot.textContent = message ? ' — ' + message : '';
        return input;
    }

    formEl.addEventListener('input', (ev) => {
        const el = ev.target;
        if (el.dataset.crlf !== undefined) { const f = fieldAt(el); if (f) f.crlf = el.checked; return; }
        if (el.dataset.fi === undefined) return;
        const f = fieldAt(el);
        if (!f) return;
        if (f.kind === 'bool') {
            f.raw = el.checked;
            const unset = el.closest('.cmp-field').querySelector('[data-unset]');
            if (unset) unset.hidden = false;
            const em = el.closest('.cmp-field').querySelector('label em');
            if (em) em.remove();
        } else {
            f.raw = el.value;
        }
        errorEl.textContent = '';
        if (f.kind === 'json' || f.kind === 'number') {
            const probe = buildAction({ name: '', fields: [f], extras: {} });
            showFieldError(el.dataset.ei, el.dataset.fi, probe.ok ? '' : probe.errors[0].message);
        }
    });
    formEl.addEventListener('change', (ev) => {
        const el = ev.target;
        if (el.classList.contains('cmp-picker')) {
            const action = actions.find((a) => a.name === el.value);
            state.entries[+el.dataset.ei] = newEntry(action);
            renderForm();
            formEl.querySelector(`#${id}-e${el.dataset.ei}-pick`).focus();
        } else if (el.tagName === 'SELECT' && el.dataset.fi !== undefined) {
            const f = fieldAt(el);
            if (f) f.raw = el.value;
        } else if (el.type === 'checkbox' && el.dataset.fi !== undefined && el.dataset.crlf === undefined) {
            // Safari fires only `change` for checkboxes.
            const f = fieldAt(el);
            if (f && f.kind === 'bool' && f.raw !== el.checked) el.dispatchEvent(new Event('input', { bubbles: true }));
        }
    });
    formEl.addEventListener('click', (ev) => {
        const el = ev.target.closest('button');
        if (!el) return;
        if (el.classList.contains('cmp-add')) {
            state.entries.push(newEntry(actions[defaultActionIndex(actions)]));
            renderForm();
            formEl.querySelector(`#${id}-e${state.entries.length - 1}-pick`).focus();
        } else if (el.classList.contains('cmp-remove')) {
            state.entries.splice(+el.dataset.ei, 1);
            renderForm();
            formEl.querySelector('.cmp-picker')?.focus();
        } else if (el.dataset.unset !== undefined) {
            const f = fieldAt(el);
            if (f) { f.raw = null; renderForm(); }
        }
    });

    rawInput.addEventListener('input', validateRaw);

    const tabs = [...root.querySelectorAll('[role=tab]')];
    for (const b of tabs) {
        b.addEventListener('click', () => showTab(b.dataset.tab));
        b.addEventListener('keydown', (ev) => {
            if (ev.key !== 'ArrowLeft' && ev.key !== 'ArrowRight') return;
            const next = tabs[(tabs.indexOf(b) + 1) % tabs.length];
            showTab(next.dataset.tab);
            next.focus();
            ev.preventDefault();
        });
    }

    function send() {
        errorEl.textContent = '';
        if (state.tab === 'raw') { onSend({ content: rawInput.value }); return; }
        const built = buildReply(req, actions, state.entries);
        if (!built.ok) {
            const ei = state.entries.indexOf(built.entry);
            let first = null;
            for (const err of built.errors) {
                const fi = built.entry.fields.findIndex((f) => f.name === err.field);
                first = first || showFieldError(ei, fi, err.message);
            }
            errorEl.textContent = `Action ${ei + 1}: ${built.errors.map((e) => e.field + ' ' + e.message).join('; ')}`;
            first?.focus();
            return;
        }
        onSend(built.reply);
    }

    root.querySelector('.llm-send').addEventListener('click', send);
    root.querySelector('.cmp-nothing').addEventListener('click', () => onSend({ content: JSON.stringify({ actions: [] }) }));
    root.querySelector('.llm-fail').addEventListener('click', () => onRefuse());
    root.addEventListener('keydown', (ev) => {
        if ((ev.metaKey || ev.ctrlKey) && ev.key === 'Enter') { ev.preventDefault(); send(); }
    });

    if (actions.length) {
        renderForm();
        showTab('form');
        if (autofocus) formEl.querySelector('.cmp-picker')?.focus({ preventScroll: true });
    } else {
        formEl.hidden = true;
        rawInput.placeholder = '{"actions": [ ... ]}';
        if (autofocus) rawInput.focus({ preventScroll: true });
    }
}
