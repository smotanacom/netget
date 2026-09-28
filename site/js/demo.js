// NetGet in the browser: the live demo on the landing page.
//
// Three machines run here, and none of them is a mock of NetGet:
//
//   1. NetGet itself, compiled to WebAssembly (crates/netget-web). Its dashboard renders
//      into an xterm.js terminal; its servers listen on a virtual network that lives inside
//      the wasm instance. About a second after it boots, this page opens a Telnet server in
//      it through the same `start_server` call the dashboard's own form uses.
//   2. A Telnet client on the same page, which connects to that server a second later
//      through NetGet.connect(port) — a real TcpStream::connect on the virtual network. It
//      edits a line locally and echoes what you type, as telnet(1) does in line mode.
//   3. The model. NetGet hands every LLM request to this page as JSON (the full prompt and
//      every action it offers). Until a model is ready, the visitor answers it through
//      ./composer.js. The model is Chrome's built-in one (the Prompt API) where the browser
//      has it, otherwise a WebLLM model on the GPU; the page switches to it by itself as soon
//      as it is loaded.
//
// The wasm bundle is built by ./web/build.sh into site/demo/pkg/.

import { mountComposer, offeredActions, entriesFromEnvelope, buildReply } from './composer.js';

const PKG = '../demo/pkg/netget_web.js';
const WEBLLM_URL = 'https://esm.run/@mlc-ai/web-llm@0.2.85';

const WEBLLM_MODELS = [
    ['Qwen2.5-1.5B-Instruct-q4f16_1-MLC', 'Qwen2.5 1.5B', '~1 GB'],
    ['Llama-3.2-3B-Instruct-q4f16_1-MLC', 'Llama 3.2 3B · better JSON', '~2 GB'],
    ['Qwen2.5-3B-Instruct-q4f16_1-MLC', 'Qwen2.5 3B', '~2 GB'],
    ['Hermes-3-Llama-3.1-8B-q4f16_1-MLC', 'Hermes 3 8B · tool calling', '~5 GB'],
];

const TELNET_PORT = 2323;
const SERVER_AFTER_MS = 1000;   // after NetGet boots, open the Telnet server
const CLIENT_AFTER_MS = 2000;   // after NetGet boots, connect the Telnet client

// Short, and answerable in one line by a small model or by a person filling in a form.
const TELNET_INSTRUCTION = 'You are the NetGet BBS, a tiny retro bulletin board reached over '
    + 'Telnet. When a visitor connects, send a short welcome banner (two lines at most) that '
    + 'ends by asking for their name. After that, answer every line they type with one or two '
    + 'short, friendly lines: greet them by name, chat, tell a one-line joke when asked, or run '
    + 'a very small text adventure if they type "play". Plain text only, under 200 characters '
    + 'per reply.';

// Options for the Prompt API: English text in, English text out.
const LM_OPTIONS = {
    expectedInputs: [{ type: 'text', languages: ['en'] }],
    expectedOutputs: [{ type: 'text', languages: ['en'] }],
};

const $ = (sel, root = document) => root.querySelector(sel);
const enc = new TextEncoder();
const dec = new TextDecoder();

const app = {
    netget: null,
    dash: null,           // xterm for the dashboard
    telnet: { term: null, conn: null, line: '', serverUp: false },
    // Who answers: 'you' until a model is ready, then 'chrome' or 'webllm'.
    answerer: 'you',
    offer: null,          // what the model control offers: 'chrome' | 'webllm' | null
    chrome: { session: null, creating: null, label: "Chrome's built-in model", badge: 'Chrome built-in' },
    webllm: { module: null, engine: null, model: null, loading: false },
    queue: [],            // model requests not yet being answered
    current: null,        // the one being answered
};

function pageTheme() {
    const t = document.documentElement.getAttribute('data-theme');
    if (t) return t;
    return window.matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark';
}

function xtermTheme() {
    const dark = pageTheme() !== 'light';
    return dark
        ? { background: '#0a0c11', foreground: '#dbe2ee', cursor: '#38bdf8', selectionBackground: '#233a55' }
        : { background: '#ffffff', foreground: '#1a2231', cursor: '#0369a1', selectionBackground: '#cfe3f5' };
}

// A terminal that fills its host element and refits whenever the host changes size. With
// `minCols`, the font shrinks (not below 6px) until that many columns fit: the dashboard
// needs 80, and a phone is narrower than 80 columns of 13px text.
function makeTerm(el, opts = {}, onFit = () => {}, { minCols = 0 } = {}) {
    const term = new window.Terminal(Object.assign({
        cursorBlink: false,
        fontFamily: "'JetBrains Mono', ui-monospace, monospace",
        fontSize: 13,
        lineHeight: 1.15,
        theme: xtermTheme(),
        scrollback: 0,
        allowProposedApi: true,
    }, opts));
    const fit = new window.FitAddon.FitAddon();
    term.loadAddon(fit);
    term.open(el);
    let last = '';
    const refit = () => {
        try { fit.fit(); } catch (e) { return; }
        if (minCols && term.cols) {
            const now = term.options.fontSize;
            const want = Math.max(6, Math.min(13, Math.floor((now * term.cols / minCols) * 2) / 2));
            if (want !== now) {
                term.options.fontSize = want;
                requestAnimationFrame(refit);
                return;
            }
        }
        const size = term.cols + 'x' + term.rows;
        if (size !== last) { last = size; onFit(term.cols, term.rows); }
    };
    refit();
    new ResizeObserver(refit).observe(el);
    // A webfont arriving changes the cell size without changing the element's size, and
    // xterm.js picks the new cell up on a later render. So after every render, if the rows no
    // longer fill the element exactly, fit again (a fit that changes nothing renders nothing).
    let queued = false;
    term.onRender?.(() => {
        if (queued) return;
        const screen = el.querySelector('.xterm-screen');
        if (!screen || !term.rows) return;
        const row = screen.offsetHeight / term.rows;
        const spare = el.clientHeight - screen.offsetHeight;
        if (spare < 0 || spare >= row) {
            queued = true;
            requestAnimationFrame(() => { queued = false; refit(); });
        }
    });
    return term;
}

// Focus a terminal without scrolling the page to it.
function focusTerm(term) {
    if (term.textarea) term.textarea.focus({ preventScroll: true });
    else term.focus();
}

function escapeHtml(s) {
    return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

function step(id, done, html) {
    const el = document.getElementById(id);
    if (!el) return;
    el.classList.toggle('is-done', done);
    if (html !== undefined) el.innerHTML = html;
}

// ---------------------------------------------------------------------------------------
// Dashboard terminal: keys from the DOM, mouse from xterm's SGR reports, bytes back in.
// ---------------------------------------------------------------------------------------

function wireDashboardInput(term, netget) {
    term.attachCustomKeyEventHandler((ev) => {
        if (ev.type !== 'keydown') return false;
        // Paste arrives through onData; browser shortcuts stay with the browser.
        if ((ev.ctrlKey || ev.metaKey) && (ev.key === 'v' || ev.key === 'V')) return false;
        if (ev.metaKey && !ev.ctrlKey) return false;
        const took = netget.key(JSON.stringify({
            key: ev.key, ctrl: ev.ctrlKey, alt: ev.altKey, shift: ev.shiftKey, meta: ev.metaKey,
        }));
        if (took) ev.preventDefault();
        return false;
    });

    const sgr = /\x1b\[<(\d+);(\d+);(\d+)([Mm])/g;
    term.onData((data) => {
        let plain = '';
        let last = 0;
        let m;
        while ((m = sgr.exec(data))) {
            plain += data.slice(last, m.index);
            last = sgr.lastIndex;
            const b = +m[1];
            const low = b & 3;
            let kind;
            if (b & 64) kind = low === 0 ? 'scrollup' : 'scrolldown';
            else if (b & 32) kind = low === 3 ? 'moved' : 'drag';
            else kind = m[4] === 'M' ? 'down' : 'up';
            netget.mouse(JSON.stringify({
                kind,
                button: ['left', 'middle', 'right'][low] || 'left',
                col: +m[2] - 1, row: +m[3] - 1,
                shift: !!(b & 4), alt: !!(b & 8), ctrl: !!(b & 16),
            }));
        }
        sgr.lastIndex = 0;
        plain += data.slice(last);
        if (plain) netget.text(plain);
    });
}

// ---------------------------------------------------------------------------------------
// Who answers, and the badges that say so.
// ---------------------------------------------------------------------------------------

function answererName() {
    if (app.answerer === 'chrome') return app.chrome.badge;
    if (app.answerer === 'webllm') return 'WebLLM · ' + app.webllm.model.replace(/-q4f16_1-MLC$/, '');
    return 'you';
}

function renderBadges() {
    const text = 'model: ' + answererName();
    const cls = app.answerer === 'you' ? 'is-you' : 'is-model';
    for (const el of [$('#model-badge'), $('#dash-model')]) {
        if (!el) continue;
        el.textContent = text;
        el.className = 'model-badge ' + cls;
    }
    const who = $('#step-model-who');
    if (who) who.textContent = app.answerer === 'you' ? 'you' : answererName();
    const until = $('#step-model-until');
    if (until) until.hidden = app.answerer !== 'you' || !app.offer;
    if (app.netget) {
        const id = app.answerer === 'webllm' ? app.webllm.model
            : app.answerer === 'chrome' ? 'chrome-built-in' : 'you';
        app.netget.set_models(JSON.stringify([id]));
        if (typeof app.netget.set_model === 'function') app.netget.set_model(id);
    }
}

// A model finished loading: it answers from now on, starting with anything still waiting
// that the visitor has not begun to answer.
function modelReady(kind) {
    app.answerer = kind;
    renderBadges();
    const cur = app.current;
    if (cur && cur.manual && !cur.touched) {
        cur.manual = false;
        dispatch(cur);
    }
}

// ---------------------------------------------------------------------------------------
// The model control: Chrome's built-in model where the browser has one, WebLLM otherwise.
// ---------------------------------------------------------------------------------------

function setStatus(html) { $('#llm-status').innerHTML = html; }

function setProgress(fraction) {
    const bar = $('#llm-progress');
    if (fraction === null) { bar.hidden = true; return; }
    bar.hidden = false;
    $('#llm-progress-bar').style.width = Math.round(Math.max(0, Math.min(1, fraction)) * 100) + '%';
}

const UNTIL = 'Until it is ready, <b>you</b> are the model: each request opens a form below.';

async function chromeAvailability() {
    if (!('LanguageModel' in self) || typeof self.LanguageModel.availability !== 'function') return 'unavailable';
    try {
        return await self.LanguageModel.availability(LM_OPTIONS);
    } catch (e) {
        try { return await self.LanguageModel.availability(); } catch (e2) { return 'unavailable'; }
    }
}

async function setupModelControl() {
    const availability = await chromeAvailability();
    if (/Edg\//.test(navigator.userAgent)) {
        app.chrome.label = "Edge's built-in model";
        app.chrome.badge = 'Edge built-in';
    }
    if (availability === 'available' || availability === 'downloadable' || availability === 'downloading') {
        setupChrome(availability);
    } else {
        setupWebLlm();
    }
    renderBadges();
}

function setupChrome(availability) {
    app.offer = 'chrome';
    $('#llm-source-name').textContent = app.chrome.label;
    const btn = $('#llm-load');
    btn.textContent = 'Download ' + app.chrome.label.replace(/'s built-in model$/, "'s model");
    btn.onclick = () => createChromeSession();
    if (availability === 'available') {
        btn.hidden = true;
        setStatus('Loading it now; it runs on this device. ' + UNTIL);
        createChromeSession();
        return;
    }
    // A download needs a user activation. The visitor's first click or keypress anywhere on
    // the page is one, so the download starts then; the button is the explicit way.
    btn.hidden = false;
    setStatus((availability === 'downloading' ? 'The browser is downloading it. ' : 'The browser downloads it once, then it runs on this device. ')
        + 'The download starts on your first click or keypress on this page. ' + UNTIL);
    const onGesture = () => {
        if (app.chrome.session || app.chrome.creating) return;
        createChromeSession();
    };
    document.addEventListener('pointerdown', onGesture, { capture: true });
    document.addEventListener('keydown', onGesture, { capture: true });
    app.chrome.stopWaiting = () => {
        document.removeEventListener('pointerdown', onGesture, { capture: true });
        document.removeEventListener('keydown', onGesture, { capture: true });
    };
    // Already downloading elsewhere: it may not need the activation at all.
    if (availability === 'downloading') createChromeSession({ quiet: true });
}

// Must be called synchronously from the gesture handler: `create()` checks the activation
// when it is called, not when it resolves.
function createChromeSession({ quiet = false } = {}) {
    if (app.chrome.session) return;
    if (app.chrome.creating) return;
    const btn = $('#llm-load');
    let options = Object.assign({}, LM_OPTIONS, {
        monitor(m) {
            m.addEventListener('downloadprogress', (e) => {
                setProgress(e.loaded);
                setStatus(`Downloading ${app.chrome.label}: ${Math.round(e.loaded * 100)}%. ` + UNTIL);
            });
        },
    });
    let pending;
    try {
        pending = self.LanguageModel.create(options);
    } catch (e) {
        pending = Promise.reject(e);
    }
    app.chrome.creating = pending;
    if (!quiet) { btn.disabled = true; setStatus(`Starting ${app.chrome.label}… ` + UNTIL); }
    pending.then((session) => {
        app.chrome.session = session;
        app.chrome.creating = null;
        app.chrome.stopWaiting?.();
        btn.hidden = true;
        setProgress(null);
        setStatus('Ready. It runs on this device and answers every request now.');
        modelReady('chrome');
    }, (e) => {
        app.chrome.creating = null;
        btn.disabled = false;
        if (quiet) return;
        console.warn('LanguageModel.create failed', e);
        setProgress(null);
        if (e && e.name === 'NotAllowedError') {
            setStatus('The browser wants a click before it downloads the model: press the button. ' + UNTIL);
        } else {
            setStatus('Could not start it (' + escapeHtml(e && e.message || e) + '). ' + UNTIL);
        }
    });
}

function setupWebLlm() {
    app.offer = 'webllm';
    $('#llm-source-name').textContent = 'WebLLM, on your GPU';
    const sel = $('#webllm-model');
    const btn = $('#llm-load');
    if (!navigator.gpu) {
        app.offer = null;
        $('#llm-source-name').textContent = 'No model runs in this browser';
        setStatus('It has neither a built-in model nor WebGPU (Chrome or Edge on a desktop, or Safari 18+, have one of them), so <b>you</b> are the model: each request opens a form below.');
        return;
    }
    sel.innerHTML = WEBLLM_MODELS.map(([id, label, size]) => `<option value="${id}">${escapeHtml(label)} · ${size}</option>`).join('');
    sel.hidden = false;
    const label = () => {
        const m = WEBLLM_MODELS.find(([id]) => id === sel.value);
        btn.textContent = 'Download ' + m[2];
    };
    sel.onchange = label;
    label();
    btn.hidden = false;
    btn.onclick = loadWebLlm;
    setStatus('Downloaded once from Hugging Face, then cached by the browser. ' + UNTIL);
}

async function loadWebLlm() {
    const btn = $('#llm-load');
    const sel = $('#webllm-model');
    btn.disabled = true;
    sel.disabled = true;
    app.webllm.loading = true;
    try {
        setStatus('Loading the WebLLM runtime… ' + UNTIL);
        setProgress(0);
        if (!app.webllm.module) app.webllm.module = await import(WEBLLM_URL);
        const model = sel.value;
        const engine = await app.webllm.module.CreateMLCEngine(model, {
            initProgressCallback: (p) => {
                if (typeof p.progress === 'number') setProgress(p.progress);
                setStatus(escapeHtml(p.text) + ' ' + UNTIL);
            },
        });
        app.webllm.engine = engine;
        app.webllm.model = model;
        btn.hidden = true;
        setProgress(null);
        setStatus('Ready, and cached in your browser for next time. It answers every request now.');
        modelReady('webllm');
    } catch (e) {
        console.error(e);
        setProgress(null);
        setStatus('Could not load the model: ' + escapeHtml(e && e.message || e) + '. ' + UNTIL);
        btn.disabled = false;
        sel.disabled = false;
    } finally {
        app.webllm.loading = false;
    }
}

// ---------------------------------------------------------------------------------------
// Requests: one at a time, and only the current one is on screen.
// ---------------------------------------------------------------------------------------

// What a request is about, for a one-line summary: the event id and the line received.
function describe(req) {
    const text = req.messages.map((m) => m.content).join('\n');
    const event = (text.match(/Event ID: ([\w.:-]+)/) || [])[1] || null;
    let detail = '';
    const at = text.lastIndexOf('Context data:');
    if (at >= 0) {
        try {
            const ctx = JSON.parse(text.slice(at + 'Context data:'.length).trim());
            const v = ctx.message ?? ctx.data ?? ctx.line ?? ctx.text;
            if (typeof v === 'string') detail = v;
        } catch (e) { /* not a JSON object */ }
    }
    if (!event && req.kind === 'chat') {
        const user = [...req.messages].reverse().find((m) => m.role === 'user');
        if (user) detail = user.content.split('\n')[0];
    }
    return { event: event || (req.kind === 'chat' ? 'chat' : req.kind), detail };
}

function handleLlmRequest(json) {
    const req = JSON.parse(json);
    return new Promise((resolve) => {
        app.queue.push({ req, resolve, touched: false, manual: false, started: performance.now() });
        pump();
    });
}

function pump() {
    if (app.current || !app.queue.length) { renderQueueCount(); return; }
    app.current = app.queue.shift();
    dispatch(app.current);
}

function finish(entry, reply) {
    if (app.current !== entry) return;
    entry.resolve(JSON.stringify(reply));
    app.current = null;
    renderIdle();
    pump();
}

function dispatch(entry) {
    if (app.answerer === 'you') answerManually(entry);
    else answerWithModel(entry);
}

function renderIdle() {
    $('#llm-current').innerHTML = `<div class="llm-idle">Nothing to answer right now. ${app.telnet.conn === null
        ? 'When the Telnet server needs to say something, the request lands here.'
        : 'Type a line in the Telnet terminal: whatever the server says back is decided here.'}</div>`;
}

function renderQueueCount() {
    const el = $('#llm-current .llm-queue');
    if (el) el.textContent = app.queue.length ? `+${app.queue.length} more waiting` : '';
}

function headHtml(entry, state) {
    const { event, detail } = describe(entry.req);
    return `<div class="llm-head">
        <span class="llm-id">#${entry.req.id}</span>
        <span class="llm-kind">${escapeHtml(event)}</span>
        ${detail ? `<span class="llm-meta">“${escapeHtml(detail.length > 80 ? detail.slice(0, 79) + '…' : detail)}”</span>` : ''}
        <span class="llm-state">${escapeHtml(state)}</span>
        <span class="llm-queue"></span>
      </div>`;
}

function promptHtml(req) {
    const n = offeredActions(req).length;
    return `<details class="llm-prompt">
        <summary>The prompt NetGet sent · ${req.messages.length} message${req.messages.length === 1 ? '' : 's'}${n ? ` · ${n} actions offered` : ''}</summary>
        ${req.messages.map((m) => `<span class="llm-role">${escapeHtml(m.role)}</span><pre>${escapeHtml(m.content)}</pre>`).join('')}
      </details>`;
}

// --- you are the model -------------------------------------------------------------------

function answerManually(entry, note = '') {
    entry.manual = true;
    const root = $('#llm-current');
    root.innerHTML = headHtml(entry, 'waiting for you')
        + (note ? `<div class="llm-note">${escapeHtml(note)}</div>` : '')
        + promptHtml(entry.req)
        + '<div class="llm-composer"></div>';
    renderQueueCount();
    const touch = () => { entry.touched = true; };
    const composerRoot = $('.llm-composer', root);
    for (const type of ['input', 'change', 'click']) composerRoot.addEventListener(type, touch);
    mountComposer(composerRoot, entry.req, {
        autofocus: false,
        onSend: (reply) => { finish(entry, reply); if (app.telnet.term) focusTerm(app.telnet.term); },
        onRefuse: () => { finish(entry, { error: 'refused by the person at the keyboard' }); },
    });
}

// --- a model -------------------------------------------------------------------------------

async function answerWithModel(entry) {
    const who = answererName();
    const { event, detail } = describe(entry.req);
    const root = $('#llm-current');
    root.innerHTML = headHtml(entry, 'the model is answering')
        + `<div class="llm-working"><span>${escapeHtml(who)} is answering <code>${escapeHtml(event)}</code>${detail ? ` for “${escapeHtml(detail.length > 60 ? detail.slice(0, 59) + '…' : detail)}”` : ''}<span class="llm-progress-text"></span></span></div>`;
    renderQueueCount();
    const progress = (text) => { const el = $('.llm-progress-text', root); if (el) el.textContent = text; };
    let reply;
    try {
        reply = app.answerer === 'chrome'
            ? await answerWithChrome(entry.req)
            : await answerWithWebLlm(entry.req, progress);
    } catch (e) {
        console.warn('the model could not answer; the visitor answers this one', e);
        if (app.current === entry) answerManually(entry, `${who} could not answer this one (${e && e.message || e}), so it is yours.`);
        return;
    }
    finish(entry, reply);
}

// --- Chrome's built-in model (the Prompt API) --------------------------------------------

class UnusableAnswer extends Error {}

// A JSON Schema that only NetGet's action envelope satisfies: `{"actions": [...]}` where each
// item is one of the offered actions, `type` pinned to its name. Tools are left out: they
// make a small model wander, and nothing a tool returns is needed to answer a Telnet line.
function responseConstraint(actions) {
    const items = actions.filter((a) => !a.tool).map((a) => {
        const schema = a.schema && typeof a.schema === 'object' ? a.schema : {};
        const properties = Object.assign({}, schema.properties || {}, { type: { type: 'string', enum: [a.name] } });
        const required = ['type', ...(Array.isArray(schema.required) ? schema.required.filter((r) => r !== 'type') : [])];
        return { type: 'object', properties, required };
    });
    return {
        type: 'object',
        properties: { actions: { type: 'array', items: items.length === 1 ? items[0] : { anyOf: items } } },
        required: ['actions'],
    };
}

// The model's text as the reply the composer would have built for the same actions, or an
// UnusableAnswer. Going through the composer's own model is the validation: every action
// must be one offered, every field must build.
function replyFromText(req, actions, text) {
    let parsed;
    try {
        parsed = JSON.parse(text);
    } catch (e) {
        const m = String(text).match(/\{[\s\S]*\}/);
        if (!m) throw new UnusableAnswer('it did not answer with JSON');
        try { parsed = JSON.parse(m[0]); } catch (e2) { throw new UnusableAnswer('it did not answer with JSON'); }
    }
    if (Array.isArray(parsed)) parsed = { actions: parsed };
    if (!parsed || !Array.isArray(parsed.actions)) throw new UnusableAnswer('its answer has no "actions" list');
    const envelope = { actions: [] };
    const tools = [];
    for (const item of [...parsed.actions, ...(Array.isArray(parsed.tools) ? parsed.tools : [])]) {
        const action = item && actions.find((a) => a.name === item.type);
        if (!action) throw new UnusableAnswer(`it chose an action that was not offered (${item && item.type})`);
        (action.tool ? tools : envelope.actions).push(item);
    }
    if (tools.length) envelope.tools = tools;
    const entries = entriesFromEnvelope(actions, envelope);
    if (!entries) throw new UnusableAnswer('its answer does not fit the offered actions');
    const built = buildReply(req, actions, entries);
    if (!built.ok) throw new UnusableAnswer('its answer is missing ' + built.errors.map((e) => e.field).join(', '));
    return built.reply;
}

async function answerWithChrome(req) {
    const base = app.chrome.session;
    if (!base) throw new Error('the model is not loaded');
    const roles = new Set(['system', 'user', 'assistant']);
    const messages = req.messages.map((m) => ({ role: roles.has(m.role) ? m.role : 'user', content: String(m.content ?? '') }));
    const last = messages.pop() || { role: 'user', content: '' };
    // A fresh conversation per request: NetGet sends the whole context every time.
    const session = messages.length || typeof base.clone !== 'function'
        ? await self.LanguageModel.create(Object.assign({}, LM_OPTIONS, messages.length ? { initialPrompts: messages } : {}))
        : await base.clone();
    try {
        const actions = offeredActions(req);
        if (!actions.length) {
            return { content: await session.prompt(last.content) };
        }
        const text = await session.prompt(last.content, {
            responseConstraint: responseConstraint(actions),
            // The prompt already describes the envelope and every action.
            omitResponseConstraintInput: true,
        });
        return replyFromText(req, actions, text);
    } finally {
        session.destroy?.();
    }
}

// --- WebLLM ----------------------------------------------------------------------------------

function toolsAsPrompt(tools) {
    const lines = tools.map((t) => {
        const f = t.function || t;
        return `- ${f.name}: ${f.description || ''}\n  parameters: ${JSON.stringify(f.parameters || {})}`;
    });
    return 'You can call these tools. To call one, reply with ONLY a JSON object of the form '
        + '{"tool_calls":[{"name":"<tool>","arguments":{...}}]} and nothing else. Otherwise reply in plain text.\n\n'
        + lines.join('\n');
}

function parseToolCallsFromText(text) {
    const m = text.match(/\{[\s\S]*"tool_calls"[\s\S]*\}/);
    if (!m) return null;
    try {
        const v = JSON.parse(m[0]);
        if (Array.isArray(v.tool_calls)) {
            return v.tool_calls.map((c) => ({
                name: c.name || c.function?.name,
                arguments: c.arguments ?? c.function?.arguments ?? {},
            }));
        }
    } catch (e) { /* not JSON after all */ }
    return null;
}

function safeJson(v) {
    if (typeof v !== 'string') return v ?? {};
    try { return JSON.parse(v); } catch (e) { return { raw: v }; }
}

// The reply goes back as the model wrote it; NetGet's own parser, repair and retry take it
// from there, as they would for Ollama.
async function answerWithWebLlm(req, progress) {
    const engine = app.webllm.engine;
    const messages = req.messages.map((m) => ({ role: m.role, content: m.content }));
    const hasTools = req.tools && req.tools.length;
    const base = { messages, temperature: 0.2, max_tokens: 1024 };

    if (hasTools) {
        // Native function calling first (Hermes / Llama 3.1 builds support it); any model
        // gets the tools described in the system prompt as a fallback.
        try {
            const res = await engine.chat.completions.create(Object.assign({}, base, {
                tools: req.tools, tool_choice: 'auto',
            }));
            const msg = res.choices[0].message;
            return {
                content: msg.content || null,
                tool_calls: (msg.tool_calls || []).map((c) => ({
                    id: c.id, name: c.function.name,
                    arguments: safeJson(c.function.arguments),
                })),
                prompt_tokens: res.usage?.prompt_tokens || 0,
                completion_tokens: res.usage?.completion_tokens || 0,
            };
        } catch (e) {
            console.warn('WebLLM: native tools unavailable, describing them in the prompt', e);
            const sys = messages.findIndex((m) => m.role === 'system');
            const note = toolsAsPrompt(req.tools);
            if (sys >= 0) messages[sys] = { role: 'system', content: messages[sys].content + '\n\n' + note };
            else messages.unshift({ role: 'system', content: note });
        }
    }

    let text = '';
    const stream = await engine.chat.completions.create(Object.assign({}, base, { messages, stream: true, stream_options: { include_usage: true } }));
    let usage = null;
    for await (const chunk of stream) {
        const delta = chunk.choices?.[0]?.delta?.content;
        if (delta) { text += delta; progress(` · ${text.length} characters so far`); }
        if (chunk.usage) usage = chunk.usage;
    }
    const reply = { content: text, prompt_tokens: usage?.prompt_tokens || 0, completion_tokens: usage?.completion_tokens || 0 };
    if (hasTools) {
        const calls = parseToolCallsFromText(text);
        if (calls) { reply.tool_calls = calls; reply.content = null; }
    }
    return reply;
}

// ---------------------------------------------------------------------------------------
// The Telnet client: NetGet.connect() on the virtual network, line mode with local echo.
// ---------------------------------------------------------------------------------------

const IAC = 255, DONT = 254, DO = 253, WONT = 252, WILL = 251, SB = 250, SE = 240;

// Refuse every option, as a plain line-mode client does (echo included: this client always
// echoes locally), and strip the commands from the byte stream.
function telnetFilter(bytes) {
    const out = [];
    const replies = [];
    let i = 0;
    while (i < bytes.length) {
        const b = bytes[i];
        if (b !== IAC) { out.push(b); i += 1; continue; }
        const cmd = bytes[i + 1];
        if (cmd === undefined) break;
        if (cmd === IAC) { out.push(IAC); i += 2; continue; }
        if (cmd === SB) {
            let j = i + 2;
            while (j < bytes.length && !(bytes[j] === IAC && bytes[j + 1] === SE)) j += 1;
            i = j + 2;
            continue;
        }
        if (cmd === DO || cmd === DONT || cmd === WILL || cmd === WONT) {
            const opt = bytes[i + 2];
            if (cmd === WILL) replies.push(IAC, DONT, opt);
            else if (cmd === DO) replies.push(IAC, WONT, opt);
            i += 3;
            continue;
        }
        i += 2;
    }
    return { data: new Uint8Array(out), replies: new Uint8Array(replies) };
}

function setTelnetState(text, up) {
    const el = $('#telnet-state');
    el.textContent = text;
    el.classList.toggle('is-up', !!up);
}

function telnetConnect() {
    const t = app.telnet;
    if (t.conn !== null) { app.netget.close(t.conn); t.conn = null; }
    t.line = '';
    t.term.write(`\r\nTrying 127.0.0.1:${TELNET_PORT}…\r\n`);
    const id = app.netget.connect(TELNET_PORT, (bytes) => {
        const { data, replies } = telnetFilter(bytes);
        if (replies.length && t.conn === id) app.netget.send(id, replies);
        if (data.length) t.term.write(dec.decode(data).replace(/(?<!\r)\n/g, '\r\n'));
    }, (reason) => {
        t.term.write(reason ? `\r\n[could not connect: ${reason}]\r\n` : '\r\n[connection closed by the server; press Enter to reconnect]\r\n');
        if (t.conn === id) t.conn = null;
        setTelnetState('closed', false);
        step('step-client', false);
        if (!app.current) renderIdle();
    });
    t.conn = id;
    t.term.write(`Connected to 127.0.0.1:${TELNET_PORT}.\r\n`);
    setTelnetState(`connected to :${TELNET_PORT}`, true);
    step('step-client', true, `The Telnet client is connected to <code>127.0.0.1:${TELNET_PORT}</code>.`);
    if (!app.current) renderIdle();
}

// telnet(1) in line mode: the line is edited and echoed here, and sent whole on Enter.
function telnetInput(d) {
    const t = app.telnet;
    if (t.conn === null) {
        if ((d === '\r' || d === '\n') && t.serverUp) telnetConnect();
        return;
    }
    for (const ch of d.replace(/\x1b\[[0-9;]*[A-Za-z~]|\x1bO./g, '')) {
        if (ch === '\r' || ch === '\n') {
            t.term.write('\r\n');
            app.netget.send(t.conn, enc.encode(t.line + '\r\n'));
            t.line = '';
        } else if (ch === '\x7f' || ch === '\b') {
            if (t.line.length) { t.line = t.line.slice(0, -1); t.term.write('\b \b'); }
        } else if (ch === '\x15') {                   // Ctrl-U: erase the line
            t.term.write('\b \b'.repeat(t.line.length));
            t.line = '';
        } else if (ch >= ' ') {
            t.line += ch;
            t.term.write(ch);
        }
    }
}

function wireTelnet() {
    const term = makeTerm($('#telnet-term'), { cursorBlink: true, scrollback: 500 });
    app.telnet.term = term;
    term.write('Telnet client. It connects to NetGet by itself in a moment.\r\n');
    term.onData(telnetInput);
}

// ---------------------------------------------------------------------------------------
// The automatic start: the Telnet server, then the client.
// ---------------------------------------------------------------------------------------

function startTelnetServer(attempt = 0) {
    app.netget.start_server(JSON.stringify({
        protocol: 'telnet', port: TELNET_PORT, instruction: TELNET_INSTRUCTION,
    }), (json) => {
        const r = JSON.parse(json);
        if (r.error) {
            // The dashboard hands over its status channel a moment after it boots.
            if (/not running yet/.test(r.error) && attempt < 40) { setTimeout(() => startTelnetServer(attempt + 1), 250); return; }
            step('step-server', false, `Could not open the Telnet server: ${escapeHtml(r.error)}`);
            return;
        }
        step('step-server', true, `NetGet opened a Telnet server on port <code>${TELNET_PORT}</code> (#${r.id} in the dashboard).`);
        refreshServers();
    });
}

function whenListening(port, then, deadline = performance.now() + 15000) {
    if (app.netget.listening_ports().includes(port)) { app.telnet.serverUp = true; then(); return; }
    if (performance.now() > deadline) { app.telnet.term.write('\r\n[the Telnet server did not come up]\r\n'); return; }
    setTimeout(() => whenListening(port, then, deadline), 100);
}

function refreshServers() {
    if (!app.netget) return;
    const udpPorts = new Set(Array.from(app.netget.bound_udp_ports()));
    app.netget.servers((json) => {
        const rows = JSON.parse(json);
        const el = $('#server-list');
        if (!rows.length) { el.innerHTML = '<span class="muted">nothing yet</span>'; return; }
        el.innerHTML = rows.map((r) => {
            const transport = udpPorts.has(r.port) ? 'udp' : 'tcp';
            return `<span class="server-chip ${r.status === 'Running' ? 'is-up' : ''}">#${r.id} ${escapeHtml(r.protocol)} ${transport}/${r.port} <small>${escapeHtml(r.status)}${transport === 'tcp' ? ' · ' + r.connections + ' conn' : ''}</small></span>`;
        }).join('');
    });
}

// ---------------------------------------------------------------------------------------

async function main() {
    const root = $('#demo');
    if (!root) return;
    const banner = $('#demo-banner');
    renderIdle();
    // Which model the control offers does not depend on NetGet; find out while it loads.
    const control = setupModelControl();
    if (!('WebAssembly' in window)) { banner.textContent = 'This browser cannot run WebAssembly.'; return; }

    let mod;
    try {
        mod = await import(PKG);
        await mod.default();
    } catch (e) {
        console.error(e);
        banner.hidden = false;
        banner.innerHTML = 'The demo bundle is not built for this deployment yet. Build it with '
            + '<code>./web/build.sh</code>; see <code>web/README.md</code>.';
        return;
    }
    banner.hidden = true;
    banner.textContent = '';

    // xterm.js measures its cell once, when it opens: let the terminal font arrive first.
    await Promise.race([
        document.fonts?.load("13px 'JetBrains Mono'").catch(() => {}),
        new Promise((resolve) => setTimeout(resolve, 3000)),
    ]);
    wireTelnet();
    let netget = null;
    const term = makeTerm($('#dash-term'), {}, (cols, rows) => netget?.resize(cols, rows), { minCols: 80 });
    app.dash = term;
    netget = new mod.NetGet({
        cols: term.cols,
        rows: term.rows,
        model: 'you',
        theme: pageTheme(),
        onOutput: (bytes) => term.write(bytes),
        onLlm: handleLlmRequest,
    });
    app.netget = netget;
    wireDashboardInput(term, netget);
    await control;
    renderBadges();

    setTimeout(() => startTelnetServer(), SERVER_AFTER_MS);
    setTimeout(() => whenListening(TELNET_PORT, () => {
        telnetConnect();
        focusTerm(app.telnet.term);
    }), CLIENT_AFTER_MS);

    setInterval(refreshServers, 2000);
    refreshServers();

    $('#theme-toggle')?.addEventListener('click', () => {
        setTimeout(() => { for (const t of [app.dash, app.telnet.term]) t.options.theme = xtermTheme(); }, 0);
    });
}

main();
