"""Windows controller for a credential-free, network-isolated WSL task worker."""

import json
import os
from pathlib import Path
import queue
import re
import subprocess
import threading


def wsl_path(path):
    path = Path(path).resolve()
    if not path.drive or not re.fullmatch(r"[A-Za-z]:", path.drive):
        raise ValueError("Expected a local Windows drive path")
    return "/mnt/" + path.drive[0].lower() + path.as_posix()[2:]


def command(data_root, output_root, worker, task_id, experiment, distro, venv):
    if not re.fullmatch(r"[a-z0-9]+_[123]", task_id):
        raise ValueError("Invalid task id")
    if not re.fullmatch(r"[A-Za-z0-9_-]+", experiment):
        raise ValueError("Invalid experiment name")
    if not venv.startswith("/opt/") or ".." in venv:
        raise ValueError("Expected an explicit /opt virtual environment")
    return [
        "wsl", "-d", distro, "--", "bwrap",
        "--unshare-all", "--die-with-parent", "--new-session", "--cap-drop", "ALL",
        "--ro-bind", "/usr", "/usr", "--symlink", "usr/bin", "/bin",
        "--ro-bind", "/lib", "/lib", "--ro-bind", "/lib64", "/lib64",
        "--ro-bind", venv, venv,
        "--ro-bind", wsl_path(data_root), "/work/data",
        "--bind", wsl_path(output_root), "/work/experiments/outputs",
        "--ro-bind", wsl_path(worker), "/worker.py",
        "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp",
        "--dir", "/etc", "--clearenv",
        "--setenv", "PATH", venv + "/bin:/usr/bin",
        "--setenv", "HOME", "/tmp", "--setenv", "LANG", "C.UTF-8",
        "--setenv", "APPWORLD_ROOT", "/work",
        "--setenv", "PYTHONDONTWRITEBYTECODE", "1",
        "--chdir", "/work", "--", venv + "/bin/python", "-I", "-u",
        "/worker.py", task_id, experiment,
    ]


class Sandbox:
    def __init__(self, data_root, output_root, task_id, experiment,
                 distro="Ubuntu-24.04", venv="/opt/laya-appworld/venv"):
        self.process = None
        self.error_log = None
        self.responses = queue.Queue()
        self.sequence = 0
        self.output_root = Path(output_root).resolve()
        self.output_root.mkdir(parents=True, exist_ok=False)
        worker = Path(__file__).with_name("appworld_worker.py")
        argv = command(data_root, self.output_root, worker, task_id, experiment, distro, venv)
        self.error_log = (self.output_root / "worker.stderr.log").open("x", encoding="utf-8")
        # Provider keys never reach wsl.exe or the Linux worker's inherited environment.
        env = {key: value for key, value in os.environ.items()
               if key.upper() in {"SYSTEMROOT", "WINDIR", "PATH", "TEMP", "TMP", "USERPROFILE", "LOCALAPPDATA"}}
        self.process = subprocess.Popen(
            argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.error_log,
            text=True, encoding="utf-8", env=env,
        )

        def read_responses():
            for line in self.process.stdout:
                self.responses.put(line)
            self.responses.put(None)

        self.reader = threading.Thread(target=read_responses, daemon=True)
        self.reader.start()
        try:
            ready = self._receive(90)
            if not ready.get("ready"):
                raise RuntimeError("Worker initialization failed")
            self.metadata = ready["metadata"]
            self.version = ready["appworld_version"]
        except BaseException:
            self.close()
            raise

    def _receive(self, timeout):
        try:
            line = self.responses.get(timeout=timeout)
        except queue.Empty:
            raise TimeoutError("Sandbox response timeout") from None
        if line is None:
            raise RuntimeError("Worker exited; inspect the local stderr log")
        return json.loads(line)

    def request(self, operation, timeout=30, **kwargs):
        self.sequence += 1
        self.process.stdin.write(json.dumps({"id": self.sequence, "op": operation, **kwargs}) + "\n")
        self.process.stdin.flush()
        response = self._receive(timeout)
        if response.get("id") != self.sequence:
            raise RuntimeError("Worker sequence mismatch")
        if not response.get("ok"):
            raise RuntimeError("Worker " + response.get("error_type", "error"))
        return response["result"]

    def close(self):
        if self.process is not None:
            try:
                self.process.stdin.close()
                self.process.wait(timeout=30)
            except (OSError, subprocess.TimeoutExpired):
                self.process.terminate()
                self.process.wait(timeout=10)
            self.process.stdout.close()
            self.reader.join(timeout=2)
        if self.error_log is not None:
            self.error_log.close()
