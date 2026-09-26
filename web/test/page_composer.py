#!/usr/bin/env python3
"""Page-level test of the "you are the model" composer, in a real headless browser.

    ./web/build.sh && python3 web/test/page_composer.py

Serves site/ from 127.0.0.1, opens the landing page in headless Chromium (Playwright), starts
the TCP quick start, connects the page's raw TCP client and sends a line. The request that
reaches the model panel must open the composer with send_tcp_data preselected and its fields
prefilled from the action's example; the test edits the data field, checks the Raw JSON tab
shows the same answer, sends, and waits for the bytes in the raw client's log. A second line
is answered with "Answer with nothing", which must complete the request without failing it.

Hermetic: every request that is not to the local server is answered by the test. xterm.js
(loaded from a CDN by the page) is replaced by a stub that satisfies the page's calls, since
nothing here reads the dashboard terminal; fonts and other third-party requests are aborted.

Not run in CI: it needs Playwright for Python and a Chromium, neither of which the wasm-web
job installs. web/test/smoke.mjs is the CI check of the same reply, without a DOM.
"""

import functools
import http.server
import os
import re
import sys
import threading

from playwright.sync_api import sync_playwright, expect

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
SITE = os.path.join(ROOT, "site")

XTERM_STUB = """
window.Terminal = class {
  constructor(opts) { this.options = Object.assign({}, opts); this.cols = 120; this.rows = 36; }
  loadAddon() {} open() {} write() {} onData() {} attachCustomKeyEventHandler() {}
  focus() {} reset() {}
};
window.FitAddon = { FitAddon: class { fit() {} } };
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


def main():
    if not os.path.exists(os.path.join(SITE, "demo", "pkg", "netget_web_bg.wasm")):
        sys.exit("site/demo/pkg is missing: run ./web/build.sh first")

    server = http.server.ThreadingHTTPServer(
        ("127.0.0.1", 0), functools.partial(Handler, directory=SITE)
    )
    threading.Thread(target=server.serve_forever, daemon=True).start()
    origin = f"http://127.0.0.1:{server.server_address[1]}"

    with sync_playwright() as p:
        browser = launch(p)
        browser_version = browser.version
        page = browser.new_page(viewport={"width": 1400, "height": 1000})
        errors = []
        page.on("pageerror", lambda e: errors.append(str(e)))

        def route(r):
            url = r.request.url
            if url.startswith(origin):
                return r.continue_()
            if "xterm" in url and url.endswith(".js"):
                return r.fulfill(status=200, content_type="text/javascript", body=XTERM_STUB)
            if url.endswith(".css"):
                return r.fulfill(status=200, content_type="text/css", body="")
            return r.abort()

        page.route("**/*", route)
        page.goto(origin + "/index.html")

        # The bundle boots and the quick starts appear.
        tcp = page.get_by_role("button", name="TCP echo on 7000")
        expect(tcp).to_be_visible(timeout=60_000)
        tcp.click()
        expect(page.locator("#server-list")).to_contain_text("tcp", timeout=30_000)

        page.locator("#raw-connect").click()
        page.locator("#raw-input").fill("hello from the page test")
        page.locator("#raw-input").press("Enter")

        # The request lands in the model panel with the composer open.
        expect(page.locator(".llm-card.is-manual").first).to_be_visible(timeout=30_000)
        first_id = page.locator(".llm-card.is-manual .llm-id").first.inner_text()
        card = page.locator(".llm-card", has=page.locator(".llm-id", has_text=re.compile("^" + first_id + "$")))
        picker = card.locator("select.cmp-picker").first
        expect(picker).to_have_value("send_tcp_data")
        expect(picker).to_be_focused()
        data = card.get_by_label("data", exact=False).first
        value = data.input_value()
        assert "220 Welcome" in value, f"data is not prefilled from the example: {value!r}"
        crlf = card.get_by_label("send line breaks as CRLF")
        expect(crlf).to_be_checked()
        encoding = card.locator(".cmp-kind-choice select").first
        expect(encoding).to_have_value("utf8")
        expect(card.locator(".cmp-schema summary").first).to_have_text("Schema and example")

        # COMPOSER_SCREENSHOT=/path.png keeps a picture of the composer as it first opens.
        if os.environ.get("COMPOSER_SCREENSHOT"):
            card.screenshot(path=os.environ["COMPOSER_SCREENSHOT"])

        # Tweak, check the raw tab agrees, come back, send.
        data.fill("hello back from the composer\n")
        card.get_by_role("tab", name="Raw JSON").click()
        raw = card.locator("textarea.cmp-raw-input")
        expect(raw).to_be_visible()
        assert '"hello back from the composer\\r\\n"' in raw.input_value(), raw.input_value()
        expect(card.locator(".cmp-raw-status")).to_contain_text("The form shows the same answer")
        card.get_by_role("tab", name="Form").click()
        expect(picker).to_be_visible()
        card.get_by_role("button", name="Send reply").click()

        expect(page.locator("#raw-log")).to_contain_text("hello back from the composer", timeout=30_000)
        expect(card).to_have_class(re.compile(r"\bis-done\b"), timeout=10_000)

        # A second request, answered with nothing: done, not failed.
        page.locator("#raw-input").fill("second line")
        page.locator("#raw-input").press("Enter")
        second = page.locator(".llm-card.is-pending.is-manual")
        expect(second).to_have_count(1, timeout=30_000)
        second_id = second.locator(".llm-id").inner_text()
        assert second_id != first_id, (first_id, second_id)
        second = page.locator(".llm-card", has=page.locator(".llm-id", has_text=re.compile("^" + second_id + "$")))
        second.get_by_role("button", name="Answer with nothing").click()
        expect(second).to_have_class(re.compile(r"\bis-done\b"), timeout=10_000)
        expect(second.locator(".llm-reply pre").first).to_have_text('{"actions":[]}')

        assert not errors, f"page errors: {errors}"
        browser.close()

    server.shutdown()
    print(f"browser: {browser_version}")
    print("ok: composer opened prefilled from the example, raw JSON stayed in sync, "
          "the edited answer reached the raw TCP client, and 'answer with nothing' completed")


if __name__ == "__main__":
    main()
