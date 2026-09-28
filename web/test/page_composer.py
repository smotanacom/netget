#!/usr/bin/env python3
"""The landing page's demo, in a real headless browser.

    ./web/build.sh && python3 web/test/page_composer.py

Serves site/ (or SITE_DIR) from 127.0.0.1 and opens the page in headless Chromium
(Playwright) eight times. Nothing is clicked before the assertions that say so. In every run the
model select sits in the LLM machine's header and no badge names a model; who answers is read
from the step list (`#step-model-who`).

1. WebGPU but no built-in model. The select lists only the six WebLLM models, each with its
   download size (and "thinks" for Qwen3), the default selected, one button naming the
   download, and nothing is downloaded. The Telnet server opens by itself and the Telnet terminal reads as a shell:
   `$ telnet localhost 2323`, then telnet's own `Trying 127.0.0.1...`, `Connected to
   localhost.`, `Escape character is '^]'.`. The connection's first request lands in the LLM
   panel as the "you are the model" composer, prefilled from the example of the protocol's
   first action; the test edits it, sends, and reads the banner in the Telnet terminal. A
   typed line is echoed locally and answered through the composer; a third is answered with
   "Answer with nothing"; a fourth with `close_connection`, after which the terminal prints
   `Connection closed by foreign host.` and a bare `$ ` prompt, and Enter there types the
   command again and reconnects. With a request open and again idle, no element of the demo
   has a scrollbar and neither a machine nor a screen overflows, at 1280x800, 1440x900,
   1920x1080 and 390x844. At the three wide sizes the LLM machine is right of the Telnet
   machine, level with it and the same height, both above NetGet, with the steps and the
   notes side by side below NetGet; at 390x844 the order down the page is steps, Telnet,
   NetGet, LLM, notes.
2. Neither a built-in model nor WebGPU: the WebLLM models are listed disabled, the visitor is
   the model, and the WebLLM runtime is never loaded.
3. A stub `LanguageModel` whose availability() is "available". The select lists "Gemini Nano
   (built into Chrome)" first; the page loads it without being asked, and it answers every
   request with no composer; its prompt() receives a responseConstraint naming the offered
   actions. An answer that does not parse falls back to the composer for that one request.
4. A stub whose promptStreaming() holds after two chunks: the partial answer is in the LLM
   panel while the Telnet terminal has nothing, and no Thinking block ever appears (Gemini
   Nano does not think); released, the answer reaches Telnet. Its next stream yields the whole
   text so far each time and gives the same answer. The constraint asks for `actions` alone.
5. The fake WebLLM's Qwen3, held inside its `<think>` block: the Thinking block shows the
   thinking and no answer exists yet; released, it folds to "Thought for N s", opens and closes
   on a click, the answer reaches Telnet and the thinking never does (with the real xterm.js,
   the dashboard's stream shows it). The request asked for enable_thinking and 2048 tokens.
   Screenshots of both, mid-stream and done, at 1440x900 and 390x844 when SCREENSHOT_DIR is set.
6. Switching, with the stub of 3 and WebGPU: choosing an uncached WebLLM model shows one button
   naming its size and downloads nothing, while Gemini Nano keeps answering; the click
   downloads it (the status counts the percentage) and it takes over; choosing Gemini Nano
   again re-uses its session (no create()) and unloads the WebLLM engine; choosing the WebLLM
   model again loads it from the cache with no click; and after a reload the choice is still
   selected (localStorage) and loads by itself.
7. A stub whose availability() is "downloadable" and whose create() refuses without a user
   activation, as Chrome's does. The page shows "Download Gemini Nano" and does not start
   the download until the visitor's first keypress in the Telnet terminal; the stub then
   reports progress, and the model takes over the request the visitor had not touched.
8. The browser's own Prompt API, if it has one: detected, named, and not downloaded unasked.

Headless Chromium has no built-in model, so the stubs are the only evidence this test can
give for the Prompt API path; they pin the page's side of it (availability, create with a
monitor, the activation rule, prompt with a responseConstraint), not Chrome's. WebLLM is
likewise a fake: the test answers the page's `import()` of the esm.run URL with a small ES
module exposing `hasModelInCache` and `CreateMLCEngine` (its "cache" is localStorage), so the
page carries no test hook; `navigator.gpu` is stubbed where WebGPU is wanted.

Hermetic by default: every request that is not to the local server is answered by the test.
xterm.js (a CDN script on the page) is replaced by a stub that keeps the DOM the test reads —
a textarea for keys, `.xterm-rows` for output — unless XTERM_DIR names a directory holding
the real `xterm.js`, `xterm.css` and `addon-fit.js` (5.5.0 / 0.10.0 from jsDelivr), which is
what the screenshots want. SCREENSHOT_DIR=/some/dir saves the demo at each size (and lets the
page's web fonts load).

Not run in CI: it needs Playwright for Python and a Chromium, neither of which the wasm-web
job installs. web/test/smoke.mjs is the CI check of the bundle, without a DOM.
"""

import functools
import http.server
import os
import re
import sys
import threading
import time

from playwright.sync_api import sync_playwright, expect

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
# SITE_DIR serves another layout of the same page, e.g. deploy.sh's staged copy, where css/,
# js/ and demo/ live under v/<hash>/. Everything this test reads goes through SITE.
SITE = os.environ.get("SITE_DIR") or os.path.join(ROOT, "site")
XTERM_DIR = os.environ.get("XTERM_DIR")
SCREENSHOT_DIR = os.environ.get("SCREENSHOT_DIR")

SIZES = [(1280, 800), (1440, 900), (1920, 1080), (390, 844)]

XTERM_STUB = r"""
window.Terminal = class {
  constructor(opts) { this.options = Object.assign({}, opts); this.cols = 80; this.rows = 24; this._on = []; this._text = ''; }
  loadAddon() {}
  open(el) {
    el.classList.add('xterm');
    if (getComputedStyle(el).position === 'static') el.style.position = 'relative';
    this.textarea = document.createElement('textarea');
    this.textarea.className = 'xterm-helper-textarea';
    this.textarea.style.cssText = 'position:absolute;left:0;top:0;width:0;height:0;opacity:0;padding:0;border:0';
    this.rowsEl = document.createElement('div');
    this.rowsEl.className = 'xterm-rows';
    this.rowsEl.style.cssText = 'white-space:pre-wrap;overflow:hidden;height:100%;font:13px monospace';
    el.append(this.textarea, this.rowsEl);
    this.textarea.addEventListener('keydown', (e) => {
      const d = e.key === 'Enter' ? '\r' : e.key === 'Backspace' ? '\x7f' : e.key.length === 1 ? e.key : '';
      if (d) { e.preventDefault(); for (const f of this._on) f(d); }
    });
  }
  write(s) {
    if (typeof s !== 'string') return;       // the dashboard's frames: nothing here reads them
    this._text = (this._text + s).slice(-6000);
    this.rowsEl.textContent = this._text.replace(/\x1b\[[0-9;?]*[A-Za-z]/g, '');
  }
  onData(f) { this._on.push(f); }
  attachCustomKeyEventHandler() {}
  focus() { this.textarea.focus(); }
  reset() { this._text = ''; }
};
window.FitAddon = { FitAddon: class { fit() {} } };
"""

# A stub of Chrome's Prompt API. `mode` is what availability() answers.
LANGUAGE_MODEL_STUB = r"""
(() => {
  const mode = %(mode)r;
  const log = window.__lm = { availability: mode, creates: 0, activeAtCreate: [], prompts: [], downloaded: false };
  const answer = (text) => {
    const event = (/Event ID: (\S+)/.exec(text) || [])[1] || '';
    let msg = '';
    try { msg = JSON.parse(text.slice(text.lastIndexOf('Context data:') + 13).trim()).message || ''; } catch (e) {}
    if (msg === 'garble') return 'I am not JSON at all';
    if (event === 'telnet_connection_opened') return JSON.stringify({ actions: [{ type: 'send_telnet_line', line: 'Hello from the stub model. Your name?' }] });
    return JSON.stringify({ actions: [{ type: 'send_telnet_line', line: 'stub model heard: ' + msg }] });
  };
  const session = (opts) => ({
    async prompt(text, o) { log.prompts.push({ text, options: o, initialPrompts: (opts && opts.initialPrompts) || null }); return answer(text); },
    async clone() { return session(opts); },
    destroy() {},
  });
  self.LanguageModel = {
    async availability() { return log.downloaded ? 'available' : mode; },
    create(opts) {
      log.creates += 1;
      const active = navigator.userActivation ? navigator.userActivation.isActive : false;
      log.activeAtCreate.push(active);
      if ((mode === 'downloadable' || mode === 'downloading') && !log.downloaded && !active) {
        return Promise.reject(new DOMException('Requires a user gesture when availability is "downloadable".', 'NotAllowedError'));
      }
      return new Promise((resolve) => {
        if (log.downloaded || mode === 'available') { resolve(session(opts)); return; }
        const target = new EventTarget();
        if (opts && opts.monitor) opts.monitor(target);
        let p = 0;
        const tick = () => {
          p = Math.min(1, p + 0.25);
          const e = new Event('downloadprogress'); e.loaded = p; target.dispatchEvent(e);
          if (p < 1) setTimeout(tick, 150); else { log.downloaded = true; resolve(session(opts)); }
        };
        setTimeout(tick, 150);
      });
    },
  };
})();
"""

# WebLLM, as the ES module the page imports from esm.run: the test serves this in its place
# (see open_page), so no production hook is involved. What is "in the browser cache" lives in
# localStorage, so it survives a reload like the real cache. `window.__webllm` is the log.
FAKE_WEBLLM = r"""
const KEY = '__fake_webllm_cache';
const cached = () => new Set(JSON.parse(localStorage.getItem(KEY) || '[]'));
const log = window.__webllm = window.__webllm || { imports: 0, cacheQueries: [], creates: [], unloads: [], prompts: [] };
log.imports += 1;
export async function hasModelInCache(id) { log.cacheQueries.push(id); return cached().has(id); }
// Qwen3 thinks: its stream is a `<think>` block and then the answer. While
// `window.__webllmHold` is true it stops inside the block, before `</think>`, until the test
// calls `window.__webllmRelease()`.
const THINK_PIECES = ['<think>\nThe visitor', ' just connected (zeta7). A BBS greets first,', ' then asks for a name.\nKeep it to two lines.\n', '</think>\n\n'];
export async function CreateMLCEngine(id, opts) {
  log.creates.push(id);
  log.requests = log.requests || [];
  const fresh = !cached().has(id);
  for (let p = 0.25; p <= 1; p += 0.25) {
    await new Promise((r) => setTimeout(r, fresh ? 200 : 20));
    opts && opts.initProgressCallback && opts.initProgressCallback({ progress: p, text: 'Fetching param cache' });
  }
  const set = cached(); set.add(id); localStorage.setItem(KEY, JSON.stringify([...set]));
  const answer = (messages) => {
    const text = messages.map((m) => m.content).join('\n');
    let msg = '';
    try { msg = JSON.parse(text.slice(text.lastIndexOf('Context data:') + 13).trim()).message || ''; } catch (e) {}
    return JSON.stringify({ actions: [{ type: 'send_telnet_line', line: `webllm ${id} heard: ${msg}` }] });
  };
  return {
    chat: { completions: { async create(req) {
      log.prompts.push(id);
      log.requests.push({ id, stream: !!req.stream, max_tokens: req.max_tokens, temperature: req.temperature, extra_body: req.extra_body || null });
      const content = answer(req.messages);
      if (!req.stream) return { choices: [{ message: { content, tool_calls: [] } }], usage: {} };
      if (!id.startsWith('Qwen3')) {
        return (async function* () { yield { choices: [{ delta: { content } }] }; yield { choices: [], usage: { prompt_tokens: 1, completion_tokens: 1 } }; })();
      }
      const pieces = [...THINK_PIECES, content.slice(0, 25), content.slice(25)];
      return (async function* () {
        for (let i = 0; i < pieces.length; i += 1) {
          if (i === 3 && window.__webllmHold) await new Promise((r) => { window.__webllmRelease = r; });
          await new Promise((r) => setTimeout(r, 40));
          yield { choices: [{ delta: { content: pieces[i] } }] };
        }
        yield { choices: [], usage: { prompt_tokens: 1, completion_tokens: 1 } };
      })();
    } } },
    async unload() { log.unloads.push(id); },
  };
}
"""

# The Prompt API streaming, as Chrome's promptStreaming() does: a ReadableStream of deltas. Every
# second stream yields the whole text so far each time instead, as early versions did; the page
# must end up with the same answer either way. The first stream stops after two chunks while
# `window.__lmHold` is true, until the test calls `window.__lmRelease()`.
STREAMING_LANGUAGE_MODEL_STUB = r"""
(() => {
  const log = window.__lm = { prompts: [], streams: 0, creates: 0 };
  window.__lmHold = true;
  const pieces = (text) => {
    const event = (/Event ID: (\S+)/.exec(text) || [])[1] || '';
    let msg = '';
    try { msg = JSON.parse(text.slice(text.lastIndexOf('Context data:') + 13).trim()).message || ''; } catch (e) {}
    if (event === 'telnet_connection_opened') {
      return ['{"actions":[{"type":"send_telnet_line",', '"line":"Hello from the ', 'streaming stub. Your name?"}', ']}'];
    }
    return ['{"actions":[{"type":', '"send_telnet_line","line":', '"stream heard: ' + msg + '"}]}'];
  };
  const session = () => ({
    async prompt() { throw new Error('the page should stream'); },
    promptStreaming(text, options) {
      log.prompts.push({ text, options });
      const parts = pieces(text);
      const cumulative = log.streams % 2 === 1;
      log.streams += 1;
      let i = 0;
      let sent = '';
      return new ReadableStream({
        async pull(controller) {
          if (i === 2 && window.__lmHold) await new Promise((r) => { window.__lmRelease = r; });
          await new Promise((r) => setTimeout(r, 40));
          if (i >= parts.length) { controller.close(); return; }
          sent += parts[i];
          controller.enqueue(cumulative ? sent : parts[i]);
          i += 1;
        },
      });
    },
    async clone() { return session(); },
    destroy() {},
  });
  self.LanguageModel = {
    async availability() { return 'available'; },
    async create() { log.creates += 1; return session(); },
  };
})();
"""

# WebGPU present, as far as the page's check (`navigator.gpu`) goes.
GPU_STUB = "Object.defineProperty(Navigator.prototype, 'gpu', { configurable: true, get() { return {}; } });"
NO_GPU = "Object.defineProperty(Navigator.prototype, 'gpu', { configurable: true, get() { return undefined; } });"

WEBLLM_LABELS = [
    "Qwen2.5 1.5B · WebLLM · ~1 GB download",
    "Qwen3 1.7B · WebLLM · thinks · ~1 GB download",
    "Llama 3.2 3B · WebLLM · ~2 GB download",
    "Qwen2.5 3B · WebLLM · ~2 GB download",
    "Qwen3 4B · WebLLM · thinks · ~2.3 GB download",
    "Hermes 3 8B · WebLLM · ~5 GB download",
]

# Every demo element that shows a scrollbar or overflows; [] is the pass.
SCROLL_CHECK = r"""
() => {
  const bad = [];
  const name = (el) => el.tagName.toLowerCase() + (el.id ? '#' + el.id : '') + (el.className && typeof el.className === 'string' ? '.' + el.className.trim().split(/\s+/).join('.') : '');
  for (const el of document.querySelectorAll('#demo, #demo *')) {
    if (!(el instanceof HTMLElement) || el.classList.contains('xterm-helper-textarea')) continue;
    const cs = getComputedStyle(el);
    if (cs.display === 'none' || cs.display === 'inline' || el.getClientRects().length === 0) continue;
    if (['SELECT', 'INPUT', 'BUTTON', 'OPTION'].includes(el.tagName)) continue;
    const bx = parseFloat(cs.borderLeftWidth) + parseFloat(cs.borderRightWidth);
    const by = parseFloat(cs.borderTopWidth) + parseFloat(cs.borderBottomWidth);
    const vbar = el.offsetWidth - el.clientWidth - bx;
    const hbar = el.offsetHeight - el.clientHeight - by;
    if (vbar > 1 || hbar > 1) bad.push(`${name(el)} shows a scrollbar (${vbar.toFixed(0)}px / ${hbar.toFixed(0)}px)`);
    if (el.matches('.machine, .screen, .llm-source, .demo-steps, .demo-notes, .term-host')
        && (el.scrollHeight > el.clientHeight + 1 || el.scrollWidth > el.clientWidth + 1)) {
      bad.push(`${name(el)} overflows: ${el.scrollWidth}x${el.scrollHeight} in ${el.clientWidth}x${el.clientHeight}`);
    }
  }
  if (document.documentElement.scrollWidth > window.innerWidth + 1) bad.push(`the page scrolls sideways (${document.documentElement.scrollWidth} > ${window.innerWidth})`);
  return bad;
}
"""


class Handler(http.server.SimpleHTTPRequestHandler):
    extensions_map = {
        **http.server.SimpleHTTPRequestHandler.extensions_map,
        ".wasm": "application/wasm",
        ".js": "text/javascript",
        ".mjs": "text/javascript",
    }

    def log_message(self, *args):
        pass


def launch(p):
    """Playwright's own Chromium if installed, else a system Chrome or Chromium."""
    try:
        return p.chromium.launch()
    except Exception as first:
        for channel in ("chrome", "chromium"):
            try:
                return p.chromium.launch(channel=channel)
            except Exception:
                pass
        for path in (
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ):
            if os.path.exists(path):
                return p.chromium.launch(executable_path=path)
        raise first


def open_page(browser, origin, size=(1280, 800), init_script=None):
    page = browser.new_page(viewport={"width": size[0], "height": size[1]})
    errors = []
    page.on("pageerror", lambda e: errors.append(str(e)))
    # The page never scrolls on its own; start where a visitor does and stay there.
    if init_script:
        page.add_init_script(init_script)

    def route(r):
        url = r.request.url
        if url.startswith(origin):
            return r.continue_()
        if "xterm" in url:
            base = url.rsplit("/", 1)[-1]
            if XTERM_DIR and os.path.exists(os.path.join(XTERM_DIR, base)):
                with open(os.path.join(XTERM_DIR, base), "rb") as f:
                    ctype = "text/css" if base.endswith(".css") else "text/javascript"
                    return r.fulfill(status=200, content_type=ctype, body=f.read())
            if url.endswith(".js"):
                return r.fulfill(status=200, content_type="text/javascript", body=XTERM_STUB)
            return r.fulfill(status=200, content_type="text/css", body="")
        if url.startswith("https://esm.run/@mlc-ai/web-llm"):
            return r.fulfill(status=200, content_type="text/javascript", body=FAKE_WEBLLM,
                             headers={"Access-Control-Allow-Origin": "*"})
        if SCREENSHOT_DIR and ("fonts.googleapis.com" in url or "fonts.gstatic.com" in url):
            return r.continue_()
        if url.endswith(".css"):
            return r.fulfill(status=200, content_type="text/css", body="")
        return r.abort()

    page.route("**/*", route)
    page.goto(origin + "/index.html")
    return page, errors


def telnet_text(page):
    # xterm.js's DOM renderer draws spaces as no-break spaces.
    return page.locator("#telnet-term .xterm-rows").inner_text().replace("\u00a0", " ")


def expect_telnet(page, needle, timeout=30):
    # xterm.js does not render while its terminal is scrolled out of view.
    page.locator("#telnet-term").scroll_into_view_if_needed()
    deadline = time.time() + timeout
    while time.time() < deadline:
        if needle in telnet_text(page):
            return
        time.sleep(0.1)
    raise AssertionError(f"{needle!r} never reached the Telnet terminal; it shows:\n{telnet_text(page)}")


def type_line(page, text):
    page.locator("#telnet-term .xterm-helper-textarea").focus()
    page.keyboard.type(text)
    page.keyboard.press("Enter")


def wait_for_autostart(page):
    """The server and the client come up with no clicks; returns seconds from the bundle
    being ready to each."""
    expect(page.locator("#demo-banner")).to_be_hidden(timeout=90_000)
    ready = time.time()
    expect(page.locator("#step-server")).to_have_class(re.compile(r"\bis-done\b"), timeout=30_000)
    server = time.time() - ready
    expect(page.locator("#telnet-state")).to_have_text(re.compile(r"^connected to :2323$"), timeout=30_000)
    client = time.time() - ready
    expect(page.locator("#server-list")).to_contain_text("telnet", timeout=10_000)
    return server, client


# Where the machines and the two text blocks are, as [left, top, right, bottom].
LAYOUT = r"""
() => Object.fromEntries(Object.entries({
  telnet: '#machine-telnet', model: '.machine-model', netget: '.machine-netget',
  steps: '#demo-steps', notes: '.demo-notes',
}).map(([k, sel]) => { const r = document.querySelector(sel).getBoundingClientRect(); return [k, [r.left, r.top, r.right, r.bottom]]; }))
"""


def layout_problems(page, w):
    """Wide: Telnet and the LLM side by side, level and the same height, NetGet under both,
    steps and notes side by side under NetGet. Narrow: steps, Telnet, NetGet, LLM, notes."""
    b = page.evaluate(LAYOUT)
    L, T, R, B = 0, 1, 2, 3
    bad = []
    def need(ok, what):
        if not ok:
            bad.append(f"{what}: " + ", ".join(f"{k} {[round(v) for v in b[k]]}" for k in b))
    if w > 900:
        need(b["model"][L] >= b["telnet"][R], "the LLM machine is not right of Telnet")
        need(abs(b["model"][T] - b["telnet"][T]) <= 1, "the LLM machine is not level with Telnet")
        need(abs((b["model"][B] - b["model"][T]) - (b["telnet"][B] - b["telnet"][T])) <= 1,
             "the LLM and Telnet machines differ in height")
        need(b["model"][B] <= b["netget"][T] and b["telnet"][B] <= b["netget"][T], "NetGet is not below both")
        need(b["netget"][L] <= b["telnet"][L] + 1 and b["netget"][R] >= b["model"][R] - 1, "NetGet is not full width")
        need(b["steps"][T] >= b["netget"][B] and b["notes"][T] >= b["netget"][B], "the steps and notes are not below NetGet")
        need(b["notes"][L] >= b["steps"][R], "the notes are not beside the steps")
    else:
        need(b["steps"][B] <= b["telnet"][T] <= b["telnet"][B] <= b["netget"][T] <= b["netget"][B]
             <= b["model"][T] <= b["model"][B] <= b["notes"][T],
             "the narrow order is not steps, Telnet, NetGet, LLM, notes")
    return bad


def check_no_scrollbars(page, label):
    problems = []
    for w, h in SIZES:
        page.set_viewport_size({"width": w, "height": h})
        page.wait_for_timeout(400)
        for p in page.evaluate(SCROLL_CHECK) + layout_problems(page, w):
            problems.append(f"{w}x{h} ({label}): {p}")
        if SCREENSHOT_DIR:
            page.locator("#demo").screenshot(path=os.path.join(SCREENSHOT_DIR, f"demo-{label}-{w}x{h}.png"))
            page.evaluate("document.querySelector('#machine-telnet').scrollIntoView({block: 'start'})")
            page.screenshot(path=os.path.join(SCREENSHOT_DIR, f"viewport-{label}-{w}x{h}.png"))
    page.set_viewport_size({"width": 1280, "height": 800})
    assert not problems, "scrollbars, overflow or layout in the demo:\n  " + "\n  ".join(problems)


# Chrome for Testing and Chrome itself expose the real Prompt API on a secure origin
# (127.0.0.1 is one), answering "downloadable" in a fresh profile. Run 1 is the browser
# that has none.
NO_LANGUAGE_MODEL = "delete self.LanguageModel;"


def model_labels(page):
    return [t.strip() for t in page.locator("#model-select option").all_text_contents()]


def check_model_header(page):
    """The model select sits in the LLM machine's header, and no badge names a model."""
    expect(page.locator(".machine-model .machine-label #model-select")).to_have_count(1)
    expect(page.locator(".model-badge, #model-badge, #dash-model")).to_have_count(0)


def expect_who(page, name, timeout=10_000):
    expect(page.locator("#step-model-who")).to_have_text(name, timeout=timeout)


def run_you_are_the_model(browser, origin):
    # WebGPU but no built-in model: the select lists only the WebLLM models, the default one
    # selected, and nothing downloads until the visitor asks.
    page, errors = open_page(browser, origin, init_script=NO_LANGUAGE_MODEL + GPU_STUB)
    server_s, client_s = wait_for_autostart(page)

    # The Telnet terminal is a shell session: the command, then telnet(1)'s own lines.
    for line in ("$ telnet localhost 2323", "Trying 127.0.0.1...", "Connected to localhost.", "Escape character is '^]'."):
        expect_telnet(page, line)
    assert "by itself in a moment" not in telnet_text(page)

    check_model_header(page)
    expect(page.locator("#model-select")).to_have_value("Qwen2.5-1.5B-Instruct-q4f16_1-MLC")
    assert model_labels(page) == WEBLLM_LABELS, model_labels(page)
    expect(page.locator("#llm-load")).to_have_text("Download Qwen2.5 1.5B · ~1 GB", timeout=10_000)
    expect(page.locator("#llm-status")).to_contain_text("Needs a click to download")
    expect(page.locator("#llm-status")).to_contain_text("you")
    expect_who(page, "you")
    assert page.evaluate("typeof window.__lm") == "undefined"

    # The connection's greeting request is the composer, prefilled from the example.
    llm = page.locator("#llm-current")
    expect(llm.locator(".llm-kind")).to_have_text("telnet_connection_opened", timeout=30_000)
    expect(llm.locator(".llm-state")).to_have_text("waiting for you")
    picker = llm.locator("select.cmp-picker")
    expect(picker).to_have_value("send_telnet_message")
    field = llm.locator(".cmp-field textarea, .cmp-field input[type=text]").first
    assert field.input_value() == "Hello\n", repr(field.input_value())
    expect(llm.get_by_label("send line breaks as CRLF")).to_be_checked()
    # The composer did not take the focus from the Telnet terminal.
    assert page.evaluate("document.activeElement && document.activeElement.closest('#telnet-term') !== null")

    check_no_scrollbars(page, "composer")

    field.fill("Welcome to the page-test BBS\nYour name? ")
    llm.get_by_role("button", name="Send reply").click()
    expect_telnet(page, "Welcome to the page-test BBS")
    expect(llm.locator(".llm-idle")).to_be_visible(timeout=10_000)
    expect(llm.locator(".llm-head")).to_have_count(0)

    # A typed line: echoed locally, then answered through the composer.
    type_line(page, "Ada")
    expect_telnet(page, "Your name? Ada")
    expect(llm.locator(".llm-kind")).to_have_text("telnet_message_received", timeout=30_000)
    expect(llm.locator(".llm-meta")).to_have_text("“Ada”")
    expect(llm.locator(".llm-head")).to_have_count(1)
    llm.locator(".cmp-field textarea, .cmp-field input[type=text]").first.fill("Hello, Ada!\n")
    llm.get_by_role("tab", name="Raw JSON").click()
    assert '"Hello, Ada!\\r\\n"' in llm.locator("textarea.cmp-raw-input").input_value()
    llm.get_by_role("tab", name="Form").click()
    llm.get_by_role("button", name="Send reply").click()
    expect_telnet(page, "Hello, Ada!")

    # A third, answered with nothing: done, not failed, and the panel goes idle.
    type_line(page, "quiet please")
    expect(llm.locator(".llm-meta")).to_have_text("“quiet please”", timeout=30_000)
    llm.get_by_role("button", name="Answer with nothing").click()
    expect(llm.locator(".llm-idle")).to_be_visible(timeout=10_000)
    expect(page.locator("#telnet-state")).to_have_text("connected to :2323")

    # The server hangs up: telnet's own line, and the shell prompt again. Enter at the prompt
    # types the command again and reconnects.
    type_line(page, "bye")
    expect(llm.locator(".llm-meta")).to_have_text("“bye”", timeout=30_000)
    llm.locator("select.cmp-picker").select_option("close_connection")
    llm.get_by_role("button", name="Send reply").click()
    expect_telnet(page, "Connection closed by foreign host.")
    expect(page.locator("#telnet-state")).to_have_text("closed", timeout=10_000)
    after = telnet_text(page).split("Connection closed by foreign host.")[-1]
    assert after.strip() == "$", f"no bare shell prompt after the hang-up: {after!r}"
    page.locator("#telnet-term .xterm-helper-textarea").focus()
    page.keyboard.press("Enter")
    expect(page.locator("#telnet-state")).to_have_text("connected to :2323", timeout=15_000)
    expect_telnet(page, "Escape character is '^]'.")
    after = telnet_text(page).split("Connection closed by foreign host.")[-1]
    for line in ("$ telnet localhost 2323", "Trying 127.0.0.1...", "Connected to localhost.", "Escape character is '^]'."):
        assert line in after, f"{line!r} missing from the reconnect: {after!r}"
    expect(llm.locator(".llm-kind")).to_have_text("telnet_connection_opened", timeout=30_000)
    llm.get_by_role("button", name="Answer with nothing").click()
    expect(llm.locator(".llm-idle")).to_be_visible(timeout=10_000)

    # Nothing was downloaded or loaded without a click; the runtime was asked what it has.
    log = page.evaluate("window.__webllm")
    assert log and log["creates"] == [], log
    assert "Qwen2.5-1.5B-Instruct-q4f16_1-MLC" in log["cacheQueries"], log

    check_no_scrollbars(page, "idle")
    assert not errors, f"page errors: {errors}"
    page.close()
    return server_s, client_s


def run_no_model_at_all(browser, origin):
    """Neither a built-in model nor WebGPU: the models are listed, disabled, and the visitor
    is the model."""
    page, errors = open_page(browser, origin, init_script=NO_LANGUAGE_MODEL + NO_GPU)
    wait_for_autostart(page)
    check_model_header(page)
    assert model_labels(page) == [l.replace("download", "").rsplit(" · ", 1)[0] + " · needs WebGPU" for l in WEBLLM_LABELS], model_labels(page)
    assert page.locator("#model-select option:not([disabled])").count() == 0
    expect(page.locator("#llm-status")).to_contain_text("neither a built-in model nor WebGPU")
    expect(page.locator("#llm-load")).to_be_hidden()
    expect_who(page, "you")
    assert page.evaluate("typeof window.__webllm") == "undefined", "the WebLLM runtime was loaded without WebGPU"
    assert not errors, f"page errors: {errors}"
    page.close()


def run_chrome_available(browser, origin):
    page, errors = open_page(browser, origin, init_script=LANGUAGE_MODEL_STUB % {"mode": "available"})
    wait_for_autostart(page)
    page.evaluate("""() => { window.__composerSeen = 0;
        new MutationObserver(() => { if (document.querySelector('#llm-current .cmp-picker')) window.__composerSeen += 1; })
          .observe(document.querySelector('#llm-current'), { childList: true, subtree: true }); }""")
    check_model_header(page)
    expect_who(page, "Gemini Nano")
    expect(page.locator("#model-select")).to_have_value("builtin")
    assert model_labels(page)[0] == "Gemini Nano (built into Chrome)", model_labels(page)
    expect(page.locator("#llm-load")).to_be_hidden()
    expect(page.locator("#llm-status")).to_contain_text("Ready: Gemini Nano")
    expect_telnet(page, "Hello from the stub model. Your name?")
    type_line(page, "Grace")
    expect_telnet(page, "stub model heard: Grace")
    assert page.evaluate("window.__composerSeen") == 0, "the composer opened although the model answered"

    prompts = page.evaluate("window.__lm.prompts")
    assert len(prompts) >= 2, prompts
    last = prompts[-1]["options"]
    names = re.findall(r'"enum":\["([a-z_]+)"\]', __import__("json").dumps(last["responseConstraint"], separators=(",", ":")))
    assert "send_telnet_line" in names and "wait_for_more" in names, names
    assert not any(n in names for n in ("web_search", "read_file")), f"tools were offered to the built-in model: {names}"
    assert last.get("omitResponseConstraintInput") is True, last
    # Each action's schema lists `type` first and exactly the action's parameters: a decoder
    # writes properties in schema order, so `type` after the parameters left only the actions
    # without a required parameter open to a model that writes "type" first, and a missing
    # `additionalProperties: false` let it write a key no action has.
    items = last["responseConstraint"]["properties"]["actions"]["items"]["anyOf"]
    for item in items:
        assert list(item["properties"])[0] == "type", item
        assert item["additionalProperties"] is False, item
    by_name = {item["properties"]["type"]["enum"][0]: item for item in items}
    assert list(by_name["send_telnet_line"]["properties"]) == ["type", "line"], by_name["send_telnet_line"]
    assert by_name["send_telnet_line"]["required"] == ["type", "line"], by_name["send_telnet_line"]
    assert list(by_name["send_telnet_prompt"]["properties"]) == ["type", "prompt"], by_name["send_telnet_prompt"]
    assert by_name["send_telnet_prompt"]["required"] == ["type"], by_name["send_telnet_prompt"]

    # An answer that does not parse: that request goes to the composer, with the reason.
    type_line(page, "garble")
    llm = page.locator("#llm-current")
    expect(llm.locator(".llm-note")).to_contain_text("Gemini Nano could not answer this one", timeout=15_000)
    expect(llm.locator("select.cmp-picker")).to_have_value("send_telnet_message")
    llm.locator(".cmp-field textarea, .cmp-field input[type=text]").first.fill("the person answered\n")
    llm.get_by_role("button", name="Send reply").click()
    expect_telnet(page, "the person answered")
    type_line(page, "and again")
    expect_telnet(page, "stub model heard: and again")

    assert not errors, f"page errors: {errors}"
    page.close()


def run_switching(browser, origin):
    """The built-in model answers; the visitor switches to a WebLLM model, which needs a
    download, then back, then to the WebLLM model again, which is cached by then; the choice
    survives a reload."""
    page, errors = open_page(browser, origin, init_script=LANGUAGE_MODEL_STUB % {"mode": "available"} + GPU_STUB)
    wait_for_autostart(page)
    check_model_header(page)
    sel = page.locator("#model-select")
    btn = page.locator("#llm-load")
    assert model_labels(page) == ["Gemini Nano (built into Chrome)"] + WEBLLM_LABELS, model_labels(page)
    expect(sel).to_have_value("builtin")
    expect_who(page, "Gemini Nano")
    expect_telnet(page, "Hello from the stub model. Your name?")

    # A WebLLM model that is not downloaded: one button naming the size, and no download.
    sel.select_option("Qwen2.5-3B-Instruct-q4f16_1-MLC")
    expect(btn).to_have_text("Download Qwen2.5 3B · ~2 GB", timeout=10_000)
    expect(btn).to_be_visible()
    expect(page.locator("#llm-status")).to_contain_text("Gemini Nano keeps answering")
    page.wait_for_timeout(1000)
    assert page.evaluate("window.__webllm.creates") == [], page.evaluate("window.__webllm")
    expect_who(page, "Gemini Nano")
    type_line(page, "still there?")
    expect_telnet(page, "stub model heard: still there?")
    check_no_scrollbars(page, "switching")

    # The click downloads it, the status counts up, and it takes over.
    btn.click()
    expect(page.locator("#llm-status")).to_contain_text(re.compile(r"Downloading Qwen2\.5 3B \d+%"), timeout=5_000)
    expect_who(page, "Qwen2.5 3B", timeout=15_000)
    expect(btn).to_be_hidden()
    expect(page.locator("#llm-status")).to_contain_text("Ready: Qwen2.5 3B")
    type_line(page, "hello qwen")
    expect_telnet(page, "webllm Qwen2.5-3B-Instruct-q4f16_1-MLC heard: hello qwen")
    expect(sel.locator("option[value='Qwen2.5-3B-Instruct-q4f16_1-MLC']")).to_have_text("Qwen2.5 3B · WebLLM · downloaded")

    # Back to the built-in model: its session is re-used (no create()), and the WebLLM
    # engine is unloaded.
    creates_before = page.evaluate("window.__lm.creates")
    sel.select_option("builtin")
    expect_who(page, "Gemini Nano")
    assert page.evaluate("window.__lm.creates") == creates_before, "switching back created a new built-in session"
    expect(btn).to_be_hidden()
    assert page.evaluate("window.__webllm.unloads") == ["Qwen2.5-3B-Instruct-q4f16_1-MLC"], page.evaluate("window.__webllm")
    type_line(page, "back again")
    expect_telnet(page, "stub model heard: back again")

    # The WebLLM model again: it is in the cache now, so it loads with no click.
    sel.select_option("Qwen2.5-3B-Instruct-q4f16_1-MLC")
    expect_who(page, "Qwen2.5 3B", timeout=15_000)
    assert page.evaluate("window.__webllm.creates") == ["Qwen2.5-3B-Instruct-q4f16_1-MLC"] * 2
    expect(btn).to_be_hidden()
    assert not errors, f"page errors: {errors}"

    # The choice is remembered: after a reload it is selected, and loads by itself.
    page.reload()
    wait_for_autostart(page)
    expect(sel).to_have_value("Qwen2.5-3B-Instruct-q4f16_1-MLC")
    expect_who(page, "Qwen2.5 3B", timeout=15_000)
    expect_telnet(page, "webllm Qwen2.5-3B-Instruct-q4f16_1-MLC heard:")
    page.close()


def panel_text(page):
    return page.locator("#llm-current").inner_text()


def shoot(page, name):
    """The LLM machine, and the whole demo, at 1440x900 and 390x844."""
    if not SCREENSHOT_DIR:
        return
    for w, h in ((1440, 900), (390, 844)):
        page.set_viewport_size({"width": w, "height": h})
        page.wait_for_timeout(300)
        page.locator(".machine-model").screenshot(path=os.path.join(SCREENSHOT_DIR, f"{name}-llm-{w}x{h}.png"))
        page.locator("#demo").screenshot(path=os.path.join(SCREENSHOT_DIR, f"{name}-demo-{w}x{h}.png"))
    page.set_viewport_size({"width": 1280, "height": 800})


def run_streaming_builtin(browser, origin):
    """The Prompt API streams: the answer shows in the panel while it is being written, before
    the Telnet terminal has it; Gemini Nano does not think, so no Thinking block ever shows."""
    page, errors = open_page(browser, origin, init_script=STREAMING_LANGUAGE_MODEL_STUB)
    page.evaluate("""() => { window.__thinkSeen = 0;
        new MutationObserver(() => { const t = document.querySelector('#llm-current .llm-think');
            if ((t && !t.hidden) || /Thinking|Thought for/.test(document.querySelector('#llm-current').textContent)) window.__thinkSeen += 1; })
          .observe(document.querySelector('#llm-current'), { childList: true, subtree: true, characterData: true, attributes: true }); }""")
    wait_for_autostart(page)
    expect_who(page, "Gemini Nano")
    llm = page.locator("#llm-current")

    # Mid-stream: two chunks of the greeting are on screen, the rest is not written yet, and
    # nothing has reached the Telnet terminal.
    answer = llm.locator(".llm-answer-body")
    expect(answer).to_contain_text('"line":"Hello from the', timeout=30_000)
    assert "streaming stub" not in answer.inner_text(), answer.inner_text()
    expect(llm.locator(".llm-state")).to_have_text("the model is answering")
    expect(llm.locator(".llm-think")).to_be_hidden()
    assert "Hello from the" not in telnet_text(page)
    check_no_scrollbars(page, "nano-streaming")
    shoot(page, "nano-mid")

    page.evaluate("window.__lmHold = false; window.__lmRelease()")
    expect_telnet(page, "Hello from the streaming stub. Your name?")
    expect(llm.locator(".llm-state")).to_have_text(re.compile(r"^answered in \d+\.\d s$"))
    expect(answer).to_contain_text("streaming stub. Your name?")
    expect(llm.locator(".llm-think")).to_be_hidden()
    check_no_scrollbars(page, "nano-answered")
    shoot(page, "nano-done")

    # The second stream yields the whole text so far each time: the same answer comes out.
    type_line(page, "Ada")
    expect_telnet(page, "stream heard: Ada")
    type_line(page, "Grace")
    expect_telnet(page, "stream heard: Grace")
    assert page.evaluate("window.__lm.streams") >= 3
    assert page.evaluate("window.__thinkSeen") == 0, "a Thinking block appeared for a model that does not think"

    # The constraint is the plain action envelope: no property the prompt does not describe.
    import json as _json
    constraint = page.evaluate("window.__lm.prompts[window.__lm.prompts.length - 1].options.responseConstraint")
    assert constraint["required"] == ["actions"], constraint
    assert "reasoning" not in _json.dumps(constraint), constraint
    assert not errors, f"page errors: {errors}"
    page.close()


def run_webllm_thinking(browser, origin):
    """Qwen3 thinks: its `<think>` text shows in the Thinking block while it is written, before
    any answer; once the answer is done the block folds to "Thought for N s" and opens again on
    a click; the answer reaches the Telnet terminal and the thinking never does."""
    page, errors = open_page(browser, origin, init_script=NO_LANGUAGE_MODEL + GPU_STUB)
    wait_for_autostart(page)
    llm = page.locator("#llm-current")
    # The visitor has the greeting until the model is ready; the model then takes it over.
    expect(llm.locator(".llm-kind")).to_have_text("telnet_connection_opened", timeout=30_000)
    sel = page.locator("#model-select")
    sel.select_option("Qwen3-1.7B-q4f16_1-MLC")
    btn = page.locator("#llm-load")
    expect(btn).to_have_text("Download Qwen3 1.7B · ~1 GB", timeout=10_000)
    page.evaluate("window.__webllmHold = true")
    btn.click()
    expect_who(page, "Qwen3 1.7B", timeout=15_000)

    # Mid-thought: the Thinking block shows the think text, no answer yet, nothing on Telnet.
    think = llm.locator(".llm-think")
    body = llm.locator(".llm-think-body")
    expect(think).to_be_visible(timeout=15_000)
    expect(body).to_contain_text("then asks for a name.")
    expect(llm.locator(".llm-think-label")).to_have_text("Thinking…")
    assert "<think>" not in body.inner_text(), body.inner_text()
    expect(llm.locator(".llm-answer")).to_be_hidden()
    assert "Hello" not in telnet_text(page)
    req = page.evaluate("window.__webllm.requests[window.__webllm.requests.length - 1]")
    assert req["stream"] and req["extra_body"] == {"enable_thinking": True} and req["max_tokens"] == 2048, req
    check_no_scrollbars(page, "qwen3-thinking")
    shoot(page, "qwen3-mid")

    page.evaluate("window.__webllmHold = false; window.__webllmRelease()")
    expect_telnet(page, "webllm Qwen3-1.7B-q4f16_1-MLC heard:")
    expect(llm.locator(".llm-think-label")).to_have_text(re.compile(r"^Thought for \d+\.\d s$"))
    expect(think).to_have_class(re.compile(r"\bis-collapsed\b"))
    expect(body).to_be_hidden()
    expect(llm.locator(".llm-think-head")).to_have_attribute("aria-expanded", "false")
    expect(llm.locator(".llm-answer-body")).to_contain_text('"send_telnet_line"')
    assert "</think>" not in llm.locator(".llm-answer-body").inner_text()
    check_no_scrollbars(page, "qwen3-thought")
    shoot(page, "qwen3-done")

    llm.locator(".llm-think-head").click()
    expect(body).to_be_visible()
    expect(body).to_contain_text("zeta7")
    expect(llm.locator(".llm-think-head")).to_have_attribute("aria-expanded", "true")
    llm.locator(".llm-think-head").click()
    expect(body).to_be_hidden()

    # A typed line: thought about, answered, and only the answer reaches the terminal.
    type_line(page, "Ada")
    expect_telnet(page, "webllm Qwen3-1.7B-q4f16_1-MLC heard: Ada")
    assert "zeta7" not in telnet_text(page) and "think>" not in telnet_text(page), telnet_text(page)
    # With the real xterm.js the dashboard renders into the DOM: its stream shows the thinking.
    if XTERM_DIR:
        page.locator("#dash-term").scroll_into_view_if_needed()
        deadline = time.time() + 15
        while "zeta7" not in page.locator("#dash-term .xterm-rows").inner_text() and time.time() < deadline:
            time.sleep(0.2)
        assert "zeta7" in page.locator("#dash-term .xterm-rows").inner_text(), "the thinking never reached the dashboard"
    assert not errors, f"page errors: {errors}"
    page.close()


def run_chrome_downloadable(browser, origin):
    page, errors = open_page(browser, origin, init_script=LANGUAGE_MODEL_STUB % {"mode": "downloadable"})
    wait_for_autostart(page)
    check_model_header(page)
    btn = page.locator("#llm-load")
    expect(btn).to_be_visible()
    expect(btn).to_have_text("Download Gemini Nano")
    assert model_labels(page)[0] == "Gemini Nano (built into Chrome)", model_labels(page)
    expect(page.locator("#llm-status")).to_contain_text("first click or keypress")
    # No interaction yet: no download, and the visitor is the model.
    llm = page.locator("#llm-current")
    expect(llm.locator(".llm-kind")).to_have_text("telnet_connection_opened", timeout=30_000)
    expect(llm.locator("select.cmp-picker")).to_be_visible()
    expect_who(page, "you")
    assert page.evaluate("window.__lm.downloaded") is False
    assert True not in page.evaluate("window.__lm.activeAtCreate"), "create() was called with an activation nobody gave"
    check_no_scrollbars(page, "downloadable")

    # The first keypress in the Telnet terminal starts the download; the model then answers
    # the greeting nobody had touched.
    page.locator("#telnet-term .xterm-helper-textarea").focus()
    page.keyboard.type("x")
    expect(page.locator("#llm-progress")).to_be_visible(timeout=5_000)
    expect(page.locator("#llm-status")).to_contain_text(re.compile(r"Downloading Gemini Nano \d+%"))
    expect_who(page, "Gemini Nano")
    assert True in page.evaluate("window.__lm.activeAtCreate")
    expect(btn).to_be_hidden()
    expect_telnet(page, "Hello from the stub model. Your name?")
    expect(llm.locator(".cmp-picker")).to_have_count(0)

    assert not errors, f"page errors: {errors}"
    page.close()


def run_real_prompt_api(browser, origin):
    """The browser's own LanguageModel, if it has one: detected, offered with the button,
    and not asked to download anything without a click or keypress (none is given)."""
    page, errors = open_page(browser, origin)
    wait_for_autostart(page)
    availability = page.evaluate(
        "async () => self.LanguageModel ? await LanguageModel.availability().catch((e) => 'error: ' + e) : null")
    if availability in ("downloadable", "downloading"):
        assert model_labels(page)[0] == "Gemini Nano (built into Chrome)", model_labels(page)
        expect(page.locator("#llm-load")).to_be_visible()
        expect(page.locator("#llm-status")).to_contain_text("first click or keypress")
        expect_who(page, "you")
        expect(page.locator("#llm-current .llm-kind")).to_have_text("telnet_connection_opened", timeout=30_000)
    assert not errors, f"page errors: {errors}"
    page.close()
    return availability


def main():
    if not any("netget_web_bg.wasm" in files for _, _, files in os.walk(SITE)):
        sys.exit(f"no netget_web_bg.wasm under {SITE}: run ./web/build.sh first")
    if SCREENSHOT_DIR:
        os.makedirs(SCREENSHOT_DIR, exist_ok=True)

    server = http.server.ThreadingHTTPServer(
        ("127.0.0.1", 0), functools.partial(Handler, directory=SITE)
    )
    threading.Thread(target=server.serve_forever, daemon=True).start()
    origin = f"http://127.0.0.1:{server.server_address[1]}"

    with sync_playwright() as p:
        browser = launch(p)
        version = browser.version
        server_s, client_s = run_you_are_the_model(browser, origin)
        run_no_model_at_all(browser, origin)
        run_chrome_available(browser, origin)
        run_streaming_builtin(browser, origin)
        run_webllm_thinking(browser, origin)
        run_switching(browser, origin)
        run_chrome_downloadable(browser, origin)
        real = run_real_prompt_api(browser, origin)
        browser.close()

    server.shutdown()
    print(f"browser: {version}; xterm: {'real, from ' + XTERM_DIR if XTERM_DIR else 'stub'}")
    print(f"ok: with no clicks the Telnet server opened {server_s:.1f}s and the client connected "
          f"{client_s:.1f}s after the bundle was ready; the terminal read as a telnet session; you "
          "answered through the composer and the answers reached the Telnet terminal; a hang-up "
          "printed telnet's line and Enter at the prompt reconnected; the machines laid out and no scrollbars at "
          + ", ".join(f"{w}x{h}" for w, h in SIZES))
    print("ok: a stub LanguageModel ('available') loaded by itself and answered with no composer, "
          "constrained to the offered actions; an unparseable answer fell back to the composer")
    print("ok: a streaming LanguageModel's answer showed in the panel before it reached Telnet, with no "
          "Thinking block, whether its chunks were deltas or the whole text so far")
    print("ok: Qwen3's <think> text showed in the Thinking block before any answer, folded to 'Thought "
          "for N s' once answered and opened on a click; only the answer reached Telnet"
          + ("; the dashboard showed the thinking" if XTERM_DIR else ""))
    print("ok: with neither a built-in model nor WebGPU the WebLLM models were listed disabled")
    print("ok: switching from Gemini Nano to an uncached WebLLM model showed its sized download "
          "button and downloaded nothing until clicked; back to Gemini Nano re-used its session; "
          "the WebLLM model again loaded from the cache with no click; the choice survived a reload")
    print("ok: a stub LanguageModel ('downloadable') waited for the first keypress, showed the "
          "button and progress, then took over the untouched request")
    print(f"real Prompt API in this browser: availability() = {real!r}"
          + ("; the page offered it with the button and started no download" if real in ("downloadable", "downloading") else ""))


if __name__ == "__main__":
    main()
