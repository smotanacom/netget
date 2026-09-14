// NetGet in the browser: the live demo on the landing page.
//
// Three things run here, and none of them is a mock of NetGet:
//
//   1. NetGet itself, compiled to WebAssembly (crates/netget-web). Its dashboard renders
//      into an xterm.js terminal; its TCP/Telnet/HTTP servers listen on a virtual network
//      that lives inside the wasm instance.
//   2. Clients for those servers, on the same page: a Telnet terminal, a "browser" that
//      speaks raw HTTP/1.1, and a netcat-style raw socket. They connect through
//      NetGet.connect(port), which is a real TcpStream::connect on that virtual network.
//   3. The model. NetGet hands every LLM request to this page as JSON (the full prompt,
//      the tools, everything). The page answers with WebLLM (a model running on your GPU
//      via WebGPU), a local Ollama you opened up to it, or — the default — you, typing.
//
// The wasm bundle is built by ./web/build.sh into docs/demo/pkg/.

const PKG = '../demo/pkg/netget_web.js';
const WEBLLM_URL = 'https://esm.run/@mlc-ai/web-llm@0.2.85';

const WEBLLM_MODELS = [
    ['Qwen2.5-1.5B-Instruct-q4f16_1-MLC', 'Qwen2.5 1.5B · ~1 GB · fast'],
    ['Llama-3.2-3B-Instruct-q4f16_1-MLC', 'Llama 3.2 3B · ~2 GB · better JSON'],
    ['Qwen2.5-3B-Instruct-q4f16_1-MLC', 'Qwen2.5 3B · ~2 GB'],
    ['Hermes-3-Llama-3.1-8B-q4f16_1-MLC', 'Hermes 3 8B · ~5 GB · tool calling'],
];

const QUICK_STARTS = [
    {
        label: 'Telnet server on 2323',
        protocol: 'telnet', port: 2323,
        instruction: 'You are a friendly BBS-style Telnet server for the NetGet project. Greet '
            + 'each visitor with a short banner, then answer whatever they type in one or two '
            + 'lines. Keep every reply under 200 characters.',
        client: 'telnet',
    },
    {
        label: 'HTTP server on 8080',
        protocol: 'http', port: 8080,
        instruction: 'You are a tiny website about the NetGet project. Serve a short HTML page '
            + 'for any path, with a heading and one paragraph that mentions the path that was '
            + 'requested. Answer /api/* paths with a small JSON object instead.',
        client: 'browser',
    },
    {
        label: 'TCP echo on 7000',
        protocol: 'tcp', port: 7000,
        instruction: 'Echo every line the client sends back to it, uppercased, followed by a '
            + 'newline. Say nothing else.',
        client: 'raw',
    },
    {
        label: 'UDP on 5555',
        protocol: 'udp', port: 5555,
        instruction: 'For every datagram, reply with one datagram: the received text reversed. '
            + 'Say nothing else.',
        client: 'udp',
    },
];

const $ = (sel, root = document) => root.querySelector(sel);
const enc = new TextEncoder();
const dec = new TextDecoder();

const app = {
    netget: null,
    dash: null,           // xterm for the dashboard
    mode: 'manual',       // 'manual' | 'webllm' | 'ollama'
    webllm: { module: null, engine: null, model: null, status: 'idle' },
    ollama: { url: 'http://localhost:11434', model: '' },
    pendingManual: new Map(),
    requestCount: 0,
    telnet: { term: null, conn: null, localEcho: true, port: 2323 },
    raw: { conn: null, port: 7000 },
    udp: { sock: null, port: 5555 },
    browser: { port: 8080 },
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

function makeTerm(el, opts = {}) {
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
    fit.fit();
    return { term, fit };
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
// The model panel.
// ---------------------------------------------------------------------------------------

function setModeBadge() {
    const badge = $('#model-badge');
    const dash = $('#dash-model');
    let text, cls;
    if (app.mode === 'webllm' && app.webllm.engine) {
        text = 'model: WebLLM · ' + app.webllm.model.replace(/-q4f16_1-MLC$/, '');
        cls = 'is-webllm';
    } else if (app.mode === 'ollama') {
        text = 'model: Ollama · ' + (app.ollama.model || '?');
        cls = 'is-ollama';
    } else {
        text = 'model: you';
        cls = 'is-you';
    }
    for (const el of [badge, dash]) {
        if (!el) continue;
        el.textContent = text;
        el.className = 'model-badge ' + cls;
    }
    if (app.netget) {
        const name = app.mode === 'webllm' && app.webllm.model ? app.webllm.model
            : app.mode === 'ollama' ? app.ollama.model || 'ollama' : 'you';
        app.netget.set_models(JSON.stringify([name]));
    }
}

function setMode(mode) {
    if (mode === 'webllm' && !app.webllm.engine) mode = 'manual';
    app.mode = mode;
    for (const r of document.querySelectorAll('input[name=llm-mode]')) r.checked = r.value === mode;
    setModeBadge();
}

function escapeHtml(s) {
    return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

function pretty(v) {
    try { return JSON.stringify(v, null, 2); } catch (e) { return String(v); }
}

function addRequestCard(req) {
    const list = $('#llm-transcript');
    $('#llm-empty')?.remove();
    const card = document.createElement('article');
    card.className = 'llm-card is-pending';
    const started = performance.now();
    const toolNames = (req.tools || []).map((t) => t.function?.name || t.name).filter(Boolean);
    card.innerHTML = `
      <header class="llm-card-head">
        <span class="llm-id">#${req.id}</span>
        <span class="llm-kind">${escapeHtml(req.kind)}</span>
        <span class="llm-meta">${req.messages.length} message${req.messages.length === 1 ? '' : 's'}${toolNames.length ? ' · ' + toolNames.length + ' tools' : ''}</span>
        <span class="llm-status">waiting for the model…</span>
      </header>
      <details class="llm-section" open>
        <summary>Prompt (what NetGet sends)</summary>
        <div class="llm-messages">
          ${req.messages.map((m) => `<div class="llm-msg llm-role-${escapeHtml(m.role)}"><span class="llm-role">${escapeHtml(m.role)}</span><pre>${escapeHtml(m.content)}</pre></div>`).join('')}
        </div>
      </details>
      ${toolNames.length ? `<details class="llm-section"><summary>Tools (${toolNames.length}): ${escapeHtml(toolNames.join(', '))}</summary><pre>${escapeHtml(pretty(req.tools))}</pre></details>` : ''}
      <div class="llm-reply"></div>
    `;
    list.prepend(card);
    const statusEl = $('.llm-status', card);
    const replyEl = $('.llm-reply', card);
    return {
        el: card,
        setStatus(text) { statusEl.textContent = text; },
        stream(text) {
            card.classList.add('is-streaming');
            let pre = $('pre.llm-stream', replyEl);
            if (!pre) {
                replyEl.innerHTML = '<div class="llm-reply-label">Reply (streaming)</div><pre class="llm-stream"></pre>';
                pre = $('pre.llm-stream', replyEl);
            }
            pre.textContent = text;
        },
        showReply(reply) {
            const ms = Math.round(performance.now() - started);
            card.classList.remove('is-pending', 'is-streaming');
            card.classList.add('is-done');
            statusEl.textContent = `answered in ${(ms / 1000).toFixed(1)}s`;
            const parts = [];
            if (reply.content) parts.push(`<div class="llm-reply-label">Reply</div><pre>${escapeHtml(reply.content)}</pre>`);
            if (reply.tool_calls && reply.tool_calls.length) parts.push(`<div class="llm-reply-label">Tool calls</div><pre>${escapeHtml(pretty(reply.tool_calls))}</pre>`);
            if (!parts.length) parts.push('<div class="llm-reply-label">Reply</div><pre class="llm-dim">(empty)</pre>');
            replyEl.innerHTML = parts.join('');
        },
        showError(err) {
            card.classList.remove('is-pending', 'is-streaming');
            card.classList.add('is-error');
            statusEl.textContent = 'failed';
            replyEl.innerHTML = `<div class="llm-reply-label">Error</div><pre>${escapeHtml(err && err.message || err)}</pre>`;
        },
        form(html) { replyEl.innerHTML = html; return replyEl; },
    };
}

async function handleLlmRequest(json) {
    const req = JSON.parse(json);
    app.requestCount += 1;
    const card = addRequestCard(req);
    try {
        let reply;
        if (app.mode === 'webllm' && app.webllm.engine) reply = await answerWithWebLlm(req, card);
        else if (app.mode === 'ollama') reply = await answerWithOllama(req, card);
        else reply = await answerManually(req, card);
        card.showReply(reply);
        return JSON.stringify(reply);
    } catch (e) {
        card.showError(e);
        return JSON.stringify({ error: String(e && e.message || e) });
    }
}

// --- you are the model ---------------------------------------------------------------

function answerManually(req, card) {
    card.setStatus('waiting for YOU');
    card.el.classList.add('is-manual');
    const toolNames = (req.tools || []).map((t) => t.function?.name || t.name).filter(Boolean);
    const hint = req.kind === 'generate'
        ? 'Reply the way the prompt above asks — usually a JSON object with an "actions" array.'
        : (toolNames.length
            ? 'This request offers tools. Reply with text, or call one: <code>[{"name":"' + escapeHtml(toolNames[0]) + '","arguments":{…}}]</code>.'
            : 'Reply with text.');
    const root = card.form(`
      <div class="llm-reply-label">Your reply</div>
      <p class="llm-hint">${hint}</p>
      <textarea class="llm-input" rows="6" placeholder='{"actions": [ ... ]}'></textarea>
      ${toolNames.length ? '<textarea class="llm-tools-input" rows="3" placeholder="Tool calls as JSON (optional)"></textarea>' : ''}
      <div class="llm-actions">
        <button class="btn btn-primary llm-send">Send reply</button>
        <button class="btn llm-fail">Refuse (fail closed)</button>
      </div>
    `);
    const ta = $('.llm-input', root);
    ta.focus();
    return new Promise((resolve, reject) => {
        $('.llm-send', root).addEventListener('click', () => {
            const reply = { content: ta.value };
            const toolsTa = $('.llm-tools-input', root);
            if (toolsTa && toolsTa.value.trim()) {
                try {
                    reply.tool_calls = JSON.parse(toolsTa.value);
                } catch (e) {
                    toolsTa.classList.add('is-invalid');
                    return;
                }
            }
            resolve(reply);
        });
        $('.llm-fail', root).addEventListener('click', () => reject(new Error('refused by the person at the keyboard')));
        ta.addEventListener('keydown', (ev) => {
            if ((ev.metaKey || ev.ctrlKey) && ev.key === 'Enter') $('.llm-send', root).click();
        });
    });
}

// --- WebLLM ----------------------------------------------------------------------------

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

async function answerWithWebLlm(req, card) {
    const engine = app.webllm.engine;
    card.setStatus('WebLLM is thinking…');
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
        if (delta) { text += delta; card.stream(text); }
        if (chunk.usage) usage = chunk.usage;
    }
    const reply = { content: text, prompt_tokens: usage?.prompt_tokens || 0, completion_tokens: usage?.completion_tokens || 0 };
    if (hasTools) {
        const calls = parseToolCallsFromText(text);
        if (calls) { reply.tool_calls = calls; reply.content = null; }
    }
    return reply;
}

function safeJson(v) {
    if (typeof v !== 'string') return v ?? {};
    try { return JSON.parse(v); } catch (e) { return { raw: v }; }
}

async function loadWebLlm() {
    const btn = $('#webllm-load');
    const status = $('#webllm-status');
    const select = $('#webllm-model');
    if (!navigator.gpu) {
        status.textContent = 'This browser has no WebGPU. Chrome or Edge on a desktop, or Safari 18+, can run models locally.';
        return;
    }
    btn.disabled = true;
    select.disabled = true;
    app.webllm.status = 'loading';
    try {
        status.textContent = 'Loading the WebLLM runtime…';
        if (!app.webllm.module) app.webllm.module = await import(WEBLLM_URL);
        const model = select.value;
        const engine = await app.webllm.module.CreateMLCEngine(model, {
            initProgressCallback: (p) => { status.textContent = p.text; },
        });
        app.webllm.engine = engine;
        app.webllm.model = model;
        app.webllm.status = 'ready';
        status.textContent = 'Ready. The model is cached in your browser for next time.';
        setMode('webllm');
    } catch (e) {
        console.error(e);
        status.textContent = 'Could not load the model: ' + (e && e.message || e);
        app.webllm.status = 'error';
        btn.disabled = false;
        select.disabled = false;
    }
}

// --- local Ollama ------------------------------------------------------------------------

async function answerWithOllama(req, card) {
    card.setStatus('asking Ollama at ' + app.ollama.url + '…');
    const body = {
        model: app.ollama.model,
        messages: req.messages.map((m) => ({ role: m.role, content: m.content })),
        stream: false,
        options: { temperature: 0.2 },
    };
    if (req.tools && req.tools.length) body.tools = req.tools;
    const res = await fetch(app.ollama.url.replace(/\/$/, '') + '/api/chat', {
        method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body),
    });
    if (!res.ok) throw new Error('Ollama answered HTTP ' + res.status + ': ' + (await res.text()).slice(0, 300));
    const v = await res.json();
    const msg = v.message || {};
    return {
        content: msg.content || null,
        tool_calls: (msg.tool_calls || []).map((c, i) => ({
            id: 'ollama-' + i, name: c.function?.name, arguments: c.function?.arguments ?? {},
        })),
        prompt_tokens: v.prompt_eval_count || 0,
        completion_tokens: v.eval_count || 0,
    };
}

async function connectOllama() {
    const status = $('#ollama-status');
    const url = $('#ollama-url').value.trim().replace(/\/$/, '');
    app.ollama.url = url;
    status.textContent = 'Checking ' + url + '…';
    try {
        const res = await fetch(url + '/api/tags');
        if (!res.ok) throw new Error('HTTP ' + res.status);
        const v = await res.json();
        const names = (v.models || []).map((m) => m.name);
        if (!names.length) throw new Error('no models pulled');
        const sel = $('#ollama-model');
        sel.innerHTML = names.map((n) => `<option>${escapeHtml(n)}</option>`).join('');
        sel.disabled = false;
        app.ollama.model = names[0];
        sel.onchange = () => { app.ollama.model = sel.value; setModeBadge(); };
        status.textContent = names.length + ' models. ';
        setMode('ollama');
    } catch (e) {
        status.innerHTML = 'Cannot reach it (' + escapeHtml(e.message || e) + '). Start Ollama with '
            + '<code>OLLAMA_ORIGINS=https://netget.net ollama serve</code> so this page may call it.';
    }
}

// ---------------------------------------------------------------------------------------
// Clients: Telnet, a browser, raw TCP. All are NetGet.connect() on the virtual network.
// ---------------------------------------------------------------------------------------

const IAC = 255, DONT = 254, DO = 253, WONT = 252, WILL = 251, SB = 250, SE = 240, OPT_ECHO = 1;

// Answer negotiation the way a plain terminal would (refuse everything but ECHO from the
// server) and strip the commands from the byte stream.
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
            if (cmd === WILL && opt === OPT_ECHO) {
                app.telnet.localEcho = false;
                $('#telnet-echo').checked = false;
                replies.push(IAC, DO, opt);
            } else if (cmd === WONT && opt === OPT_ECHO) {
                app.telnet.localEcho = true;
                $('#telnet-echo').checked = true;
                replies.push(IAC, DONT, opt);
            } else if (cmd === WILL) {
                replies.push(IAC, DONT, opt);
            } else if (cmd === DO) {
                replies.push(IAC, WONT, opt);
            }
            i += 3;
            continue;
        }
        i += 2;
    }
    return { data: new Uint8Array(out), replies: new Uint8Array(replies) };
}

function telnetConnect() {
    const t = app.telnet;
    if (t.conn !== null) { app.netget.close(t.conn); t.conn = null; }
    const port = parseInt($('#telnet-port').value, 10) || t.port;
    t.port = port;
    t.term.reset();
    t.term.write(`Trying 127.0.0.1:${port}…\r\n`);
    const id = app.netget.connect(port, (bytes) => {
        const { data, replies } = telnetFilter(bytes);
        if (replies.length && t.conn === id) app.netget.send(id, replies);
        if (data.length) t.term.write(dec.decode(data).replace(/(?<!\r)\n/g, '\r\n'));
    }, (reason) => {
        t.term.write(reason ? `\r\n[could not connect: ${reason}]\r\n` : '\r\n[connection closed by the server]\r\n');
        if (t.conn === id) t.conn = null;
        $('#telnet-connect').textContent = 'Connect';
    });
    t.conn = id;
    t.term.write('Connected.\r\n');
    $('#telnet-connect').textContent = 'Reconnect';
    t.term.focus();
}

function wireTelnet() {
    const { term } = makeTerm($('#telnet-term'), { cursorBlink: true, scrollback: 500 });
    app.telnet.term = term;
    term.write('Telnet client. Start a Telnet server in the dashboard, then Connect.\r\n');
    term.onData((d) => {
        const t = app.telnet;
        if (t.conn === null) return;
        const wire = d.replace(/\r(?!\n)/g, '\r\n');
        app.netget.send(t.conn, enc.encode(wire));
        if (t.localEcho) term.write(d === '\r' ? '\r\n' : d === '\x7f' ? '\b \b' : d);
    });
    $('#telnet-connect').addEventListener('click', telnetConnect);
    $('#telnet-echo').addEventListener('change', (ev) => { app.telnet.localEcho = ev.target.checked; });
}

// --- the browser -------------------------------------------------------------------------

function parseHttpResponse(bytes) {
    const text = dec.decode(bytes);
    const sep = text.indexOf('\r\n\r\n');
    if (sep < 0) return { status: '(incomplete response)', headers: [], body: text, contentType: 'text/plain' };
    const head = text.slice(0, sep).split('\r\n');
    const status = head.shift();
    const headers = head.map((l) => { const i = l.indexOf(':'); return [l.slice(0, i).trim(), l.slice(i + 1).trim()]; });
    const get = (name) => (headers.find((h) => h[0].toLowerCase() === name) || [])[1] || '';
    let bodyBytes = bytes.slice(enc.encode(text.slice(0, sep + 4)).length);
    if (/chunked/i.test(get('transfer-encoding'))) bodyBytes = dechunk(bodyBytes);
    return { status, headers, body: dec.decode(bodyBytes), contentType: get('content-type') };
}

function dechunk(bytes) {
    const out = [];
    let i = 0;
    while (i < bytes.length) {
        let j = i;
        while (j < bytes.length && !(bytes[j] === 13 && bytes[j + 1] === 10)) j += 1;
        const size = parseInt(dec.decode(bytes.slice(i, j)).split(';')[0], 16);
        if (!size) break;
        out.push(...bytes.slice(j + 2, j + 2 + size));
        i = j + 2 + size + 2;
    }
    return new Uint8Array(out);
}

function browse() {
    const bar = $('#browser-url');
    let url = bar.value.trim();
    const m = url.match(/^(?:https?:\/\/)?(?:localhost|127\.0\.0\.1)?(?::(\d+))?(\/.*)?$/i);
    if (!m) { renderPage({ status: 'Only http://localhost:<port>/<path> exists on this network.', headers: [], body: '' }); return; }
    const port = m[1] ? parseInt(m[1], 10) : app.browser.port;
    const path = m[2] || '/';
    app.browser.port = port;
    bar.value = `http://localhost:${port}${path}`;
    const chunks = [];
    const win = $('#browser-window');
    win.classList.add('is-loading');
    const started = performance.now();
    const id = app.netget.connect(port, (bytes) => chunks.push(bytes), (reason) => {
        win.classList.remove('is-loading');
        if (reason) { renderPage({ status: 'Connection failed: ' + reason, headers: [], body: '' }); return; }
        const total = chunks.reduce((n, c) => n + c.length, 0);
        const all = new Uint8Array(total);
        let off = 0;
        for (const c of chunks) { all.set(c, off); off += c.length; }
        const res = parseHttpResponse(all);
        res.ms = Math.round(performance.now() - started);
        renderPage(res);
    });
    const request = `GET ${path} HTTP/1.1\r\nHost: localhost:${port}\r\nUser-Agent: NetGet-demo-browser/1.0\r\nAccept: text/html,application/json,*/*\r\nConnection: close\r\n\r\n`;
    app.netget.send(id, enc.encode(request));
    app.netget.close(id);
}

function renderPage(res) {
    $('#browser-status').textContent = res.status + (res.ms != null ? ` · ${res.ms} ms` : '');
    $('#browser-headers').textContent = res.headers.map((h) => h.join(': ')).join('\n');
    const frame = $('#browser-frame');
    const pre = $('#browser-pre');
    if (/html/i.test(res.contentType || '')) {
        frame.hidden = false; pre.hidden = true;
        frame.srcdoc = res.body;
    } else {
        frame.hidden = true; pre.hidden = false;
        let body = res.body;
        if (/json/i.test(res.contentType || '')) { try { body = JSON.stringify(JSON.parse(body), null, 2); } catch (e) { /* as is */ } }
        pre.textContent = body;
    }
}

function wireBrowser() {
    $('#browser-go').addEventListener('click', browse);
    $('#browser-url').addEventListener('keydown', (ev) => { if (ev.key === 'Enter') browse(); });
}

// --- raw TCP ------------------------------------------------------------------------------

function rawLog(kind, text) {
    const log = $('#raw-log');
    const line = document.createElement('div');
    line.className = 'raw-line raw-' + kind;
    line.textContent = text;
    log.appendChild(line);
    log.scrollTop = log.scrollHeight;
}

function printable(bytes) {
    let s = '';
    for (const b of bytes) {
        if (b === 10) s += '\n';
        else if (b >= 32 && b < 127) s += String.fromCharCode(b);
        else if (b === 13 || b === 9) s += b === 9 ? '\t' : '';
        else s += '\\x' + b.toString(16).padStart(2, '0');
    }
    return s;
}

function wireRaw() {
    $('#raw-connect').addEventListener('click', () => {
        if (app.raw.conn !== null) { app.netget.close(app.raw.conn); app.raw.conn = null; }
        const port = parseInt($('#raw-port').value, 10) || app.raw.port;
        app.raw.port = port;
        rawLog('sys', `connecting to 127.0.0.1:${port}`);
        const id = app.netget.connect(port, (bytes) => rawLog('in', printable(bytes)), (reason) => {
            rawLog('sys', reason ? 'could not connect: ' + reason : 'closed by the server');
            if (app.raw.conn === id) app.raw.conn = null;
        });
        app.raw.conn = id;
    });
    $('#raw-close').addEventListener('click', () => {
        if (app.raw.conn === null) return;
        app.netget.close(app.raw.conn);
        rawLog('sys', 'closed our end');
        app.raw.conn = null;
    });
    const send = () => {
        if (app.raw.conn === null) { rawLog('sys', 'not connected'); return; }
        const input = $('#raw-input');
        const text = input.value + ($('#raw-newline').checked ? '\n' : '');
        app.netget.send(app.raw.conn, enc.encode(text));
        rawLog('out', printable(enc.encode(text)));
        input.value = '';
    };
    $('#raw-send').addEventListener('click', send);
    $('#raw-input').addEventListener('keydown', (ev) => { if (ev.key === 'Enter') send(); });
}

// --- raw UDP ------------------------------------------------------------------------------

function udpLog(kind, text) {
    const log = $('#udp-log');
    const line = document.createElement('div');
    line.className = 'raw-line raw-' + kind;
    line.textContent = text;
    log.appendChild(line);
    log.scrollTop = log.scrollHeight;
}

function hexBytes(text) {
    const clean = text.replace(/[^0-9a-fA-F]/g, '');
    const out = new Uint8Array(Math.floor(clean.length / 2));
    for (let i = 0; i < out.length; i += 1) out[i] = parseInt(clean.substr(i * 2, 2), 16);
    return out;
}

function wireUdp() {
    const send = () => {
        if (app.udp.sock === null) {
            app.udp.sock = app.netget.udp_open((bytes, fromPort) => {
                udpLog('in', `:${fromPort} → ` + printable(bytes));
            });
            udpLog('sys', 'opened a UDP socket on the virtual network');
        }
        const port = parseInt($('#udp-port').value, 10) || app.udp.port;
        app.udp.port = port;
        const input = $('#udp-input');
        const bytes = $('#udp-hex').checked ? hexBytes(input.value) : enc.encode(input.value);
        app.netget.udp_send(app.udp.sock, port, bytes);
        udpLog('out', `→ :${port} ` + printable(bytes));
        input.value = '';
    };
    $('#udp-send').addEventListener('click', send);
    $('#udp-input').addEventListener('keydown', (ev) => { if (ev.key === 'Enter') send(); });
}

// --- tabs, quick starts, server list ---------------------------------------------------

function wireTabs() {
    for (const tabs of document.querySelectorAll('.tabs')) {
        tabs.addEventListener('click', (ev) => {
            const btn = ev.target.closest('[data-tab]');
            if (!btn) return;
            showTab(tabs, btn.dataset.tab);
        });
    }
}

function showTab(tabs, name) {
    for (const b of tabs.querySelectorAll('[data-tab]')) b.classList.toggle('is-active', b.dataset.tab === name);
    const panelRoot = tabs.parentElement;
    for (const p of panelRoot.querySelectorAll('[data-panel]')) p.hidden = p.dataset.panel !== name;
    if (name === 'telnet') { app.telnet.term.focus(); }
}

function refreshServers() {
    if (!app.netget) return;
    const udpPorts = new Set(Array.from(app.netget.bound_udp_ports()));
    app.netget.servers((json) => {
        const rows = JSON.parse(json);
        const el = $('#server-list');
        if (!rows.length) { el.innerHTML = '<span class="muted">nothing yet — use a quick start, or press <kbd>a</kbd> in the dashboard</span>'; return; }
        el.innerHTML = rows.map((r) => {
            const transport = udpPorts.has(r.port) ? 'udp' : 'tcp';
            return `<span class="server-chip ${r.status === 'Running' ? 'is-up' : ''}">#${r.id} ${escapeHtml(r.protocol)} ${transport}/${r.port} <small>${escapeHtml(r.status)}${transport === 'tcp' ? ' · ' + r.connections + ' conn' : ''}</small></span>`;
        }).join('');
    });
}

function wireQuickStarts() {
    const box = $('#quick-starts');
    for (const q of QUICK_STARTS) {
        const btn = document.createElement('button');
        btn.className = 'btn';
        btn.textContent = q.label;
        btn.title = q.instruction;
        btn.addEventListener('click', () => {
            btn.disabled = true;
            app.netget.start_server(JSON.stringify({ protocol: q.protocol, port: q.port, instruction: q.instruction }), (json) => {
                const r = JSON.parse(json);
                btn.disabled = false;
                if (r.error) { alert(r.error); return; }
                refreshServers();
                const tabs = $('#client-tabs');
                showTab(tabs, q.client);
                if (q.client === 'telnet') $('#telnet-port').value = q.port;
                if (q.client === 'browser') $('#browser-url').value = `http://localhost:${q.port}/`;
                if (q.client === 'raw') $('#raw-port').value = q.port;
                if (q.client === 'udp') $('#udp-port').value = q.port;
            });
        });
        box.appendChild(btn);
    }
}

// ---------------------------------------------------------------------------------------

async function main() {
    const root = $('#demo');
    if (!root) return;
    const banner = $('#demo-banner');
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

    const { term, fit } = makeTerm($('#dash-term'));
    app.dash = term;
    const netget = new mod.NetGet({
        cols: term.cols,
        rows: term.rows,
        model: 'you',
        theme: pageTheme(),
        onOutput: (bytes) => term.write(bytes),
        onLlm: handleLlmRequest,
    });
    app.netget = netget;
    wireDashboardInput(term, netget);
    new ResizeObserver(() => { fit.fit(); netget.resize(term.cols, term.rows); }).observe($('#dash-term'));
    term.focus();

    wireTabs();
    wireTelnet();
    wireBrowser();
    wireRaw();
    wireUdp();
    wireQuickStarts();

    for (const r of document.querySelectorAll('input[name=llm-mode]')) {
        r.addEventListener('change', () => setMode(r.value));
    }
    const sel = $('#webllm-model');
    sel.innerHTML = WEBLLM_MODELS.map(([id, label]) => `<option value="${id}">${label}</option>`).join('');
    $('#webllm-load').addEventListener('click', loadWebLlm);
    $('#ollama-connect').addEventListener('click', connectOllama);
    if (!navigator.gpu) $('#webllm-status').textContent = 'No WebGPU in this browser; WebLLM needs Chrome/Edge on a desktop or Safari 18+.';
    setMode('manual');
    setInterval(refreshServers, 2000);
    refreshServers();

    $('#theme-toggle')?.addEventListener('click', () => {
        setTimeout(() => { for (const t of [app.dash, app.telnet.term]) t.options.theme = xtermTheme(); }, 0);
    });
}

main();
