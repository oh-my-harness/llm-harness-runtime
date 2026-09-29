"""Pinned MiniWoB HTML/JS over an owned browser; native grading, no solution policy."""

from contextlib import contextmanager
from functools import partial
import hashlib
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import math
from pathlib import Path
import subprocess
import threading


REVISION = "33c3b4ddef8c6eb67c57a29663d844b1eda7e614"
FAMILIES = ("click-button", "click-button-sequence", "click-checkboxes", "choose-list", "enter-text", "enter-text-2")
HUD_CSS = "#reward-display,#click-canvas,#attention-canvas{display:none!important}"


def verify_checkout(checkout):
    checkout = Path(checkout).resolve()
    head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=checkout, check=True, capture_output=True, text=True).stdout.strip()
    dirty = subprocess.run(["git", "status", "--porcelain", "--untracked-files=all", "--", "miniwob/html"],
                           cwd=checkout, check=True, capture_output=True, text=True).stdout.strip()
    if head != REVISION or dirty:
        raise ValueError("MiniWoB checkout must be the clean pinned revision")
    root = checkout / "miniwob/html"
    files = [root / "miniwob" / (name + ".html") for name in FAMILIES]
    for directory in ("core", "common"):
        files.extend(p for p in (root / directory).rglob("*") if p.is_file())
    return {"revision": head, "asset_sha256": {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest()
                                              for p in sorted(files)}}


@contextmanager
def serve_checkout(checkout):
    class Handler(SimpleHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def list_directory(self, _path):
            self.send_error(403)
            return None

    root = Path(checkout).resolve() / "miniwob/html"
    server = ThreadingHTTPServer(("127.0.0.1", 0), partial(Handler, directory=str(root)))
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield "http://127.0.0.1:" + str(server.server_port)
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


def native_result(metadata):
    reward = metadata["raw_reward"]
    if type(metadata["done"]) is not bool or type(reward) not in (int, float) or not math.isfinite(reward):
        raise ValueError("Invalid native MiniWoB result")
    return {**metadata, "success": metadata["done"] and reward == 1.0}


class MiniWobEpisode:
    def __init__(self, seed, timeout_seconds):
        self.seed, self.timeout_seconds = seed, timeout_seconds

    def prepare(self, browser):
        page = browser.page
        page.wait_for_function("typeof core !== 'undefined' && core.cover_div !== null && WOB_TASK_READY")
        # The upstream task is 160x210; its debug HUD is outside that task region.
        # Hiding it prevents grade leakage and countdown-only stale-page detections in both arms.
        page.add_style_tag(content=HUD_CSS)
        result = page.evaluate("""({seed, timeout_ms}) => {
            const original_timeout_ms = core.EPISODE_MAX_TIME;
            core.EPISODE_MAX_TIME = timeout_ms;
            Math.seedrandom(seed);
            core.setDataMode('train');
            core.startEpisodeReal();
            return {goal:core.getUtterance(), original_timeout_ms,
                    timeout_ms:core.EPISODE_MAX_TIME, mode:WOB_DATA_MODE, seed};
        }""", {"seed": self.seed, "timeout_ms": self.timeout_seconds * 1000})
        page.wait_for_function("WOB_TASK_READY && !WOB_DONE_GLOBAL")
        if not isinstance(result["goal"], str) or not result["goal"].strip():
            raise ValueError("Expected a public text task instruction")
        return result

    def terminated(self, browser):
        # No grade or hidden expected value is read while the policy can still act.
        done = browser.page.evaluate("WOB_DONE_GLOBAL")
        if type(done) is not bool:
            raise ValueError("Missing native termination signal")
        return done

    def evaluate(self, browser):
        return native_result(browser.page.evaluate("""() => ({
            done:WOB_DONE_GLOBAL, raw_reward:WOB_RAW_REWARD_GLOBAL,
            env_reward:WOB_REWARD_GLOBAL, reason:WOB_REWARD_REASON
        })"""))
