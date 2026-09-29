"""AppWorld worker. Run only inside the restricted bubblewrap mount namespace."""

import contextlib
import importlib.metadata
import json
import os
from pathlib import Path
import resource
import sys


def main():
    if os.environ.get("APPWORLD_ROOT") != "/work" or Path("/mnt/d").exists():
        raise RuntimeError("The worker must run in the benchmark sandbox")
    resource.setrlimit(resource.RLIMIT_AS, (6 * 1024**3, 6 * 1024**3))
    resource.setrlimit(resource.RLIMIT_NOFILE, (512, 512))
    task_id, experiment_name = sys.argv[1:3]
    with contextlib.redirect_stdout(sys.stderr):
        from appworld import AppWorld
        from appworld.evaluator import evaluate_task

        world = AppWorld(
            task_id=task_id, experiment_name=experiment_name,
            load_ground_truth=False, max_interactions=30,
            max_api_calls_per_interaction=100, timeout_seconds=10,
            raise_on_unsafe_syntax=True, null_patch_unsafe_execution=True,
            random_seed=20260929,
        )
    docs = world.task.api_docs
    metadata = {
        "instruction": world.task.instruction,
        "supervisor": world.task.supervisor,
        "app_descriptions": world.task.app_descriptions,
        "api_docs": docs,
    }
    # No Task.ground_truth, solution, evaluation code, or difficulty enters metadata.
    print(json.dumps({
        "ready": True, "metadata": metadata,
        "appworld_version": importlib.metadata.version("appworld"),
    }), flush=True)
    evaluated = False
    closed = False
    try:
        for line in sys.stdin:
            request = json.loads(line)
            try:
                with contextlib.redirect_stdout(sys.stderr):
                    operation = request["op"]
                    if operation == "close":
                        break
                    if operation == "evaluate":
                        if not closed:
                            world.close()
                            closed = True
                        tracker = evaluate_task(task_id, experiment_name, suppress_errors=True)
                        if tracker.num_tests <= 0:
                            raise ValueError("Evaluation produced no tests")
                        result = {
                            "success": tracker.success,
                            "tests_passed": tracker.pass_count,
                            "tests_total": tracker.num_tests,
                        }
                        evaluated = True
                    elif evaluated or closed:
                        raise ValueError("Evaluation ends the attempt")
                    elif operation == "execute":
                        output = world.execute(request["code"])
                        result = {
                            "output": output,
                            "execution_failed": output.startswith("Execution failed."),
                            "task_completed": world.task_completed(),
                        }
                    elif operation == "docs":
                        app = request["app"]
                        api = request.get("api")
                        if api is None:
                            result = {
                                name: entry["description"]
                                for name, entry in docs[app].items()
                            }
                        else:
                            result = docs[app][api]
                    else:
                        raise ValueError("Unknown worker operation")
                reply = {"id": request["id"], "ok": True, "result": result}
            except Exception as error:
                # Worker has no provider credentials; keep errors separate from model observations.
                reply = {"id": request["id"], "ok": False, "error_type": type(error).__name__}
                print(f"Worker failure: {type(error).__name__}", file=sys.stderr)
            print(json.dumps(reply), flush=True)
    finally:
        if not closed:
            with contextlib.redirect_stdout(sys.stderr):
                world.close()


if __name__ == "__main__":
    main()
