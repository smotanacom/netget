#!/usr/bin/env python3
"""The landing page's demo, in a real headless browser.

    ./web/build.sh && python3 web/test/page_composer.py

Serves site/ from 127.0.0.1 and opens the page in headless Chromium (Playwright) three times.
Nothing is clicked before the assertions that say so.

1. No built-in model (the common browser). The Telnet server opens by itself and the Telnet
   client connects by itself. The connection's first request (`telnet_connection_opened`)
   lands in the LLM panel as the "you are the model" composer, prefilled from the example of
   the protocol's first action; the test edits it, sends, and reads the banner in the Telnet
   terminal. A typed line is echoed locally and its request is answered through the
   composer too; a third is answered with "Answer with nothing". Only the current request is
   ever on screen. With a request open and again idle, no element of the demo has a
   scrollbar and neither a machine nor a screen overflows, at 1280x800, 1440x900, 1920x1080
   and 390x844.
2. A stub `LanguageModel` global whose availability() is "available". The page loads it
   without being asked, switches the badges to it, and it answers every request with no
   composer; its prompt() receives a responseConstraint naming the offered actions. An
   answer that does not parse falls back to the composer for that one request.
3. A stub whose availability() is "downloadable" and whose create() refuses without a user
   activation, as Chrome's does. The page shows the download button and does not start the
   download until the visitor's first keypress in the Telnet terminal; the stub then
   reports progress, and the model takes over the request the visitor had not touched.

Headless Chromium has no built-in model, so the stubs are the only evidence this test can
give for the Chrome path; they pin the page's side of the Prompt API (availability, create
with a monitor, the activation rule, prompt with a responseConstraint), not Chrome's.

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
SITE = os.path.join(ROOT, "site")
XTERM_DIR = os.environ.get("XTERM_DIR")
SCREENSHOT_DIR = os.environ.get("SCREENSHOT_DIR")

SIZES = [(1280, 800), (1440, 900), (1920, 1080), (390, 844)]

XTERM_STUB = r"""
window.Terminal = class {
  constructor(opts) { this.options = Object.assign({}, opts); this.cols = 80; this.rows = 24; this._on = []; this._text = ''; }
  loadAddon() {}
  open(el) {
    el.classList.add('xterm');
    el.style.position = 'relative';
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


def check_no_scrollbars(page, label):
    problems = []
    for w, h in SIZES:
        page.set_viewport_size({"width": w, "height": h})
        page.wait_for_timeout(400)
        for p in page.evaluate(SCROLL_CHECK):
            problems.append(f"{w}x{h} ({label}): {p}")
        if SCREENSHOT_DIR:
            page.locator("#demo").screenshot(path=os.path.join(SCREENSHOT_DIR, f"demo-{label}-{w}x{h}.png"))
            page.evaluate("document.querySelector('#machine-telnet').scrollIntoView({block: 'start'})")
            page.screenshot(path=os.path.join(SCREENSHOT_DIR, f"viewport-{label}-{w}x{h}.png"))
    page.set_viewport_size({"width": 1280, "height": 800})
    assert not problems, "scrollbars or overflow in the demo:\n  " + "\n  ".join(problems)


# Chrome for Testing and Chrome itself expose the real Prompt API on a secure origin
# (127.0.0.1 is one), answering "downloadable" in a fresh profile. Run 1 is the browser
# that has none.
NO_LANGUAGE_MODEL = "delete self.LanguageModel;"


def run_you_are_the_model(browser, origin):
    page, errors = open_page(browser, origin, init_script=NO_LANGUAGE_MODEL)
    server_s, client_s = wait_for_autostart(page)

    # No model: the control offers WebLLM with the default preselected, or says there is none.
    expect(page.locator("#model-badge")).to_have_text("model: you")
    expect(page.locator("#dash-model")).to_have_text("model: you")
    if page.evaluate("!!navigator.gpu"):
        expect(page.locator("#webllm-model")).to_have_value("Qwen2.5-1.5B-Instruct-q4f16_1-MLC")
        expect(page.locator("#llm-load")).to_have_text("Download ~1 GB")
    else:
        expect(page.locator("#llm-source-name")).to_have_text("No model runs in this browser")
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

    check_no_scrollbars(page, "idle")
    assert not errors, f"page errors: {errors}"
    page.close()
    return server_s, client_s


def run_chrome_available(browser, origin):
    page, errors = open_page(browser, origin, init_script=LANGUAGE_MODEL_STUB % {"mode": "available"})
    wait_for_autostart(page)
    page.evaluate("""() => { window.__composerSeen = 0;
        new MutationObserver(() => { if (document.querySelector('#llm-current .cmp-picker')) window.__composerSeen += 1; })
          .observe(document.querySelector('#llm-current'), { childList: true, subtree: true }); }""")
    expect(page.locator("#model-badge")).to_have_text("model: Chrome built-in", timeout=10_000)
    expect(page.locator("#dash-model")).to_have_text("model: Chrome built-in")
    expect(page.locator("#llm-source-name")).to_have_text("Chrome's built-in model")
    expect(page.locator("#llm-load")).to_be_hidden()
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

    # An answer that does not parse: that request goes to the composer, with the reason.
    type_line(page, "garble")
    llm = page.locator("#llm-current")
    expect(llm.locator(".llm-note")).to_contain_text("could not answer this one", timeout=15_000)
    expect(llm.locator("select.cmp-picker")).to_have_value("send_telnet_message")
    llm.locator(".cmp-field textarea, .cmp-field input[type=text]").first.fill("the person answered\n")
    llm.get_by_role("button", name="Send reply").click()
    expect_telnet(page, "the person answered")
    type_line(page, "and again")
    expect_telnet(page, "stub model heard: and again")

    assert not errors, f"page errors: {errors}"
    page.close()


def run_chrome_downloadable(browser, origin):
    page, errors = open_page(browser, origin, init_script=LANGUAGE_MODEL_STUB % {"mode": "downloadable"})
    wait_for_autostart(page)
    btn = page.locator("#llm-load")
    expect(btn).to_be_visible()
    expect(btn).to_have_text("Download Chrome's model")
    expect(page.locator("#llm-status")).to_contain_text("first click or keypress")
    # No interaction yet: no download, and the visitor is the model.
    llm = page.locator("#llm-current")
    expect(llm.locator(".llm-kind")).to_have_text("telnet_connection_opened", timeout=30_000)
    expect(llm.locator("select.cmp-picker")).to_be_visible()
    expect(page.locator("#model-badge")).to_have_text("model: you")
    assert page.evaluate("window.__lm.downloaded") is False
    assert True not in page.evaluate("window.__lm.activeAtCreate"), "create() was called with an activation nobody gave"
    check_no_scrollbars(page, "downloadable")

    # The first keypress in the Telnet terminal starts the download; the model then answers
    # the greeting nobody had touched.
    page.locator("#telnet-term .xterm-helper-textarea").focus()
    page.keyboard.type("x")
    expect(page.locator("#llm-progress")).to_be_visible(timeout=5_000)
    expect(page.locator("#model-badge")).to_have_text("model: Chrome built-in", timeout=10_000)
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
        expect(page.locator("#llm-source-name")).to_have_text("Chrome's built-in model")
        expect(page.locator("#llm-load")).to_be_visible()
        expect(page.locator("#llm-status")).to_contain_text("first click or keypress")
        expect(page.locator("#model-badge")).to_have_text("model: you")
        expect(page.locator("#llm-current .llm-kind")).to_have_text("telnet_connection_opened", timeout=30_000)
    assert not errors, f"page errors: {errors}"
    page.close()
    return availability


def main():
    if not os.path.exists(os.path.join(SITE, "demo", "pkg", "netget_web_bg.wasm")):
        sys.exit("site/demo/pkg is missing: run ./web/build.sh first")
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
        run_chrome_available(browser, origin)
        run_chrome_downloadable(browser, origin)
        real = run_real_prompt_api(browser, origin)
        browser.close()

    server.shutdown()
    print(f"browser: {version}; xterm: {'real, from ' + XTERM_DIR if XTERM_DIR else 'stub'}")
    print(f"ok: with no clicks the Telnet server opened {server_s:.1f}s and the client connected "
          f"{client_s:.1f}s after the bundle was ready; you answered through the composer and the "
          "answers reached the Telnet terminal; no scrollbars at "
          + ", ".join(f"{w}x{h}" for w, h in SIZES))
    print("ok: a stub LanguageModel ('available') loaded by itself and answered with no composer, "
          "constrained to the offered actions; an unparseable answer fell back to the composer")
    print("ok: a stub LanguageModel ('downloadable') waited for the first keypress, showed the "
          "button and progress, then took over the untouched request")
    print(f"real Prompt API in this browser: availability() = {real!r}"
          + ("; the page offered it with the button and started no download" if real in ("downloadable", "downloading") else ""))


if __name__ == "__main__":
    main()
