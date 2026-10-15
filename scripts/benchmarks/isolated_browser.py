"""Owned Chromium context, no user profile/cookies; pinned Jev DOM execution over CDP."""

from collections import Counter
import os
import time
from urllib.parse import urlsplit

from jev_reference.browser import Browser


def origin(url):
    parsed = urlsplit(url)
    if parsed.scheme not in {"http", "https"} or not parsed.hostname or parsed.username or parsed.password:
        raise ValueError("Only explicit HTTP(S) task origins are allowed")
    return parsed.scheme + "://" + parsed.netloc


class MeteredSession:
    def __init__(self, session):
        self.session, self.calls, self.failures, self.elapsed_ms = session, Counter(), 0, 0.0

    def send(self, method, params):
        self.calls[method] += 1
        start = time.perf_counter_ns()
        try:
            return self.session.send(method, params)
        except Exception:
            self.failures += 1
            raise
        finally:
            self.elapsed_ms += (time.perf_counter_ns() - start) / 1e6

    def detach(self):
        self.session.detach()


class IsolatedBrowser(Browser):
    def __init__(self, url, allowed_origins, *, headless=True):
        from playwright.sync_api import sync_playwright

        allowed = {origin(item) for item in allowed_origins}
        if origin(url) not in allowed:
            raise ValueError("Task URL is not in the frozen origin allowlist")
        self.playwright = self.chrome = self.context = None
        self.blocked_requests = 0
        try:
            self.playwright = sync_playwright().start()
            browser_env = {name: value for name, value in os.environ.items() if name.upper() in {
                "SYSTEMROOT", "WINDIR", "PATH", "TEMP", "TMP", "TMPDIR", "HOME", "USERPROFILE",
                "LOCALAPPDATA", "APPDATA", "PROGRAMFILES", "PROGRAMFILES(X86)", "COMSPEC",
                "DISPLAY", "WAYLAND_DISPLAY", "XDG_RUNTIME_DIR", "LANG", "LC_ALL",
            }}
            self.chrome = self.playwright.chromium.launch(
                headless=headless, chromium_sandbox=True, env=browser_env,
            )
            self.context = self.chrome.new_context(
                viewport={"width": 1120, "height": 780}, service_workers="block", accept_downloads=False,
            )

            def route(request):
                try:
                    permitted = origin(request.request.url) in allowed
                except ValueError:
                    permitted = False
                if permitted:
                    request.continue_()
                else:
                    self.blocked_requests += 1
                    request.abort()

            self.context.route("**/*", route)
            self.context.route_web_socket("**/*", lambda socket: socket.close())
            self.page = self.context.new_page()
            self.context.on("page", lambda page: page.close() if page != self.page else None)
            self.page.on("dialog", lambda dialog: dialog.dismiss())
            self.page.set_default_timeout(5000)
            self.page.goto(url, wait_until="load", timeout=15000)
            super().__init__(MeteredSession(self.context.new_cdp_session(self.page)))
            self.call("Emulation.setFocusEmulationEnabled", enabled=True)
        except Exception:
            self.close()
            raise

    def close(self):
        context, chrome, playwright = self.context, self.chrome, self.playwright
        self.context = self.chrome = self.playwright = None
        try:
            if context:
                context.close()
        finally:
            try:
                if chrome:
                    chrome.close()
            finally:
                if playwright:
                    playwright.stop()


def validate_acceptance(spec):
    if not isinstance(spec, dict) or not spec or set(spec) - {"url_equals", "visible_text", "fields"}:
        raise ValueError("Independent acceptance needs explicit supported assertions")
    if "url_equals" in spec:
        origin(spec["url_equals"])
    if "visible_text" in spec and (not isinstance(spec["visible_text"], list)
                                    or not all(isinstance(s, str) and s.strip() for s in spec["visible_text"])):
        raise ValueError("Visible text assertions must be nonempty strings")
    if "fields" in spec and not isinstance(spec["fields"], list):
        raise ValueError("Field assertions must be a list")
    for field in spec.get("fields", []):
        if (not isinstance(field, dict) or set(field) != {"selector", "value"}
                or not isinstance(field["selector"], str) or not field["selector"]
                or not isinstance(field["value"], str)):
            raise ValueError("Invalid acceptance field assertion")
    if not spec.get("url_equals") and not spec.get("visible_text") and not spec.get("fields"):
        raise ValueError("Empty acceptance would falsely pass every task")


def evaluate(page, spec):
    """Only trusted task assertions use selectors. Never expose assertions to the policy."""
    validate_acceptance(spec)
    checks = []
    if "url_equals" in spec:
        checks.append(page.url == spec["url_equals"])
    for text in spec.get("visible_text", []):
        matches = page.get_by_text(text, exact=True)
        checks.append(any(matches.nth(i).is_visible() for i in range(matches.count())))
    for field in spec.get("fields", []):
        element = page.locator(field["selector"])
        checks.append(element.count() == 1 and element.is_visible()
                      and element.input_value() == field["value"])
    return {"success": all(checks), "checks": checks, "passed": sum(checks), "total": len(checks)}
