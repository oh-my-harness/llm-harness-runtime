"""Local Qwen control for the author's Suite A; no task-specific decisions."""

import json
import math
import os
import statistics
import time
import traceback


def fixed_decision_page(agent, unsafe):
    page = agent.state["page"]
    excluded = agent.state.get("excluded") or set()
    safe = os.environ.get("JEV_SAFE") == "1"
    undo = agent._undo_guard()
    if not excluded and not safe and not undo:
        return page
    return {**page, "actions": [a for a in page["actions"]
            if a["id"] not in excluded and not (safe and unsafe.search(a.get("label", "")))
            and not (undo is not None and undo(a))]}


def parse_output(content, targets, controls, exclude_ops=()):
    value = json.loads(content)
    if not isinstance(value, dict) or set(value) != {"operation", "target", "text"}:
        raise ValueError("Invalid Qwen control response")
    operation, target, text = value["operation"], value["target"], value["text"]
    if type(target) is int:
        target = str(target)
    allowed = (set(targets) | set(controls) | {"DONE", "BLOCKED"}) - set(exclude_ops)
    if not isinstance(operation, str) or operation not in allowed:
        raise ValueError("Qwen chose an unavailable operation")
    if operation in targets:
        if not isinstance(target, str) or target not in targets[operation]:
            raise ValueError("Qwen chose an unavailable target")
        action = targets[operation][target]
    else:
        if target is not None:
            raise ValueError("Control operation must have a null target")
        action = controls.get(operation)
    if operation == "TYPE_TEXT":
        if not isinstance(text, str) or not text.strip() or len(text) > 2000:
            raise ValueError("Qwen returned invalid field text")
    elif text is not None:
        raise ValueError("Text is only valid for TYPE_TEXT")
    return operation, target, text, action


class QwenControl:
    def __init__(self, model, rules, emit):
        self.model, self.rules, self.emit = model, rules, emit
        self.pending = None

    def choose(self, page, goal, history, exclude_ops=(), escalate=None):
        self.pending = None
        elements, targets, controls = self.model.action_space(page["actions"])
        operations = sorted((set(targets) | set(controls) | {"DONE", "BLOCKED"}) - set(exclude_ops))
        instructions = (self.rules + '\nReturn one JSON object with exactly three keys: operation, target, text. '
                        'operation must be an exact offered operation name. target must be an exact string index '
                        'from valid_targets[operation], or the JSON literal null for a control operation. '
                        'Never use a label, element id, boolean, or the string "null" as target. '
                        'For TYPE_TEXT, supply the exact field text in this same '
                        'response. For SELECT choose an observed option index, not just its field. For controls, '
                        'DONE and BLOCKED use target=null. For every operation except TYPE_TEXT use text=null. '
                        'Only use offered operations and target indices. Never invent personal information, '
                        'selectors, code or unavailable values. If a required value is unknown, do not fill it.')
        payload = {"goal": goal, "page": {k: page[k] for k in ("url", "title", "text")},
                   "operations": operations, "elements": elements,
                   "valid_targets": {op: list(values) for op, values in targets.items() if op in operations},
                   "controls": {k: v["label"] for k, v in controls.items()},
                   "field_hints": {k: self.model.field_context(goal, a, page, history)["field"]
                                   for k, a in targets.get("TYPE_TEXT", {}).items()},
                   "recent_actions": [{k: h.get(k) for k in ("action", "kind", "text", "page_changed", "url", "title")}
                                      for h in history[-20:]]}
        started = time.perf_counter()
        result = self.model.post_json(os.environ["TEXT_MODEL_BASE_URL"] + "/chat/completions",
                                      os.environ["TEXT_MODEL_API_KEY"], {
            "model": os.environ["TEXT_MODEL"], "temperature": 0, "max_tokens": 1024,
            "response_format": {"type": "json_object"},
            "messages": [{"role": "system", "content": instructions},
                         {"role": "user", "content": json.dumps(payload, ensure_ascii=False, separators=(",", ":"))}]})
        op, target, text, action = parse_output(result["choices"][0]["message"]["content"], targets, controls, exclude_ops)
        choice = action["id"] if action is not None else op
        latency = (time.perf_counter() - started) * 1000
        if op == "TYPE_TEXT":
            self.pending = (self.model.field_context(goal, action, page, history), text,
                            {"model": os.environ["TEXT_MODEL"], "latency_ms": 0, "usage": {}, "merged": True})
        self.emit("qwen_decision", operation=op, target=target, choice=choice, text=text,
                  merged_generation=op == "TYPE_TEXT", elapsed_ms=latency,
                  request=payload, usage=result.get("usage"))
        # The executor requires a probability map; these are interface markers, not calibrated model probabilities.
        return {"goal_done": None, "choice": choice, "operation": op, "target": target, "confidence": 1.0,
                "probabilities": {choice: 1.0}, "operation_probabilities": {op: 1.0},
                "target_probabilities": {target: 1.0} if target else {}, "target_confidence": None,
                "raw_answers": {"operation": {"choice": op}}, "model": os.environ["TEXT_MODEL"],
                "usage": result.get("usage"), "latency_ms": round(latency), "request": payload,
                "probabilities_are_interface_markers": True}

    def field_text(self, context):
        if self.pending is None or self.pending[0] != context:
            raise ValueError("Qwen merged field value does not match this action")
        _, text, helper = self.pending
        return text, helper


def install(agent_module, model_module, state, emit):
    original_choose, original_field, original_select = agent_module.choose, agent_module.field_text, agent_module.select_option
    control = QwenControl(model_module, agent_module.NEXT_ACTION if hasattr(agent_module, "NEXT_ACTION")
                          else model_module.NEXT_ACTION, emit)

    def choose(*args, **kwargs):
        if state["arm"] != "A":
            return original_choose(*args, **kwargs)
        before, state["purpose"] = state["purpose"], "control_decision"
        try:
            return control.choose(*args, **kwargs)
        finally:
            state["purpose"] = before

    agent_module.choose = choose
    agent_module.field_text = lambda context: control.field_text(context) if state["arm"] == "A" else original_field(context)
    agent_module.select_option = lambda *a, **kw: None if state["arm"] == "A" else original_select(*a, **kw)

    class PairedAgent(agent_module.Agent):
        def _decision_page(self):
            return fixed_decision_page(self, agent_module.UNSAFE)

        def command(self, *args, **kwargs):
            try:
                if time.perf_counter() >= state["deadline"]:
                    raise TimeoutError("Attempt time budget exhausted at operation boundary")
                return super().command(*args, **kwargs)
            except Exception as error:
                emit("agent_error", error_type=type(error).__name__,
                     frames=[{"file": frame.filename, "line": frame.lineno, "function": frame.name}
                             for frame in traceback.extract_tb(error.__traceback__)])
                raise
    return PairedAgent


def schedule(tasks, repeat_index=0):
    return [(task, arm) for index, task in enumerate(tasks)
            for arm in (("A", "B") if (index + repeat_index) % 2 == 0 else ("B", "A"))]


def accepted(author_pass, status, task, page):
    if not author_pass or status.startswith("error:"):
        return False
    if task == "internet-checkbox":
        boxes = [a for a in page.get("actions", []) if a.get("role") == "checkbox"]
        return bool(boxes) and boxes[0].get("checked") in (True, "true")
    return True


def distribution(values):
    values = sorted(values)
    return {"n": len(values), "sum": sum(values), "mean": statistics.mean(values) if values else None,
            "median": statistics.median(values) if values else None,
            "p95": values[math.ceil(len(values) * .95) - 1] if values else None}


def compare(rows):
    groups = {}
    for arm in ("A", "B"):
        group = [r for r in rows if r["arm"] == arm]
        successes = [r for r in group if r["pass"]]
        counts = {k: None if any(r["counts"][k] is None for r in group) else sum(r["counts"][k] for r in group)
                  for k in group[0]["counts"]} if group else {}
        groups[arm] = {"completed": len(group), "passed": len(successes), "counts": counts,
                       "wall": distribution([r["attempt_wall_seconds"] for r in group]),
                       "success_wall": distribution([r["attempt_wall_seconds"] for r in successes]),
                       "failure_wall": distribution([r["attempt_wall_seconds"] for r in group if not r["pass"]])}
    pairs = {}
    for row in rows:
        key = (row["task"], row.get("repeat", 1))
        pair = pairs.setdefault(key, {})
        if row["arm"] in pair:
            raise ValueError("Duplicate task/repeat/arm")
        pair[row["arm"]] = row
    matrix = {"both_pass": 0, "only_A": 0, "only_B": 0, "both_fail": 0, "incomplete": 0}
    matched = []
    for (task, repeat), pair in pairs.items():
        if set(pair) != {"A", "B"}:
            matrix["incomplete"] += 1
            continue
        a, b = pair["A"], pair["B"]
        matrix["both_pass" if a["pass"] and b["pass"] else "only_A" if a["pass"] else "only_B" if b["pass"] else "both_fail"] += 1
        if a["pass"] and b["pass"]:
            matched.append({"task": task, "A_seconds": a["attempt_wall_seconds"], "B_seconds": b["attempt_wall_seconds"],
                            "A_qwen": a["counts"]["qwen_http"], "B_qwen": b["counts"]["qwen_http"]})
            if "repeat" in a:
                matched[-1]["repeat"] = repeat
    total_a = sum(p["A_seconds"] for p in matched)
    total_b = sum(p["B_seconds"] for p in matched)
    return {"groups": groups, "quality_pairs": matrix, "both_success": matched,
            "paired_speed_ratio_A_over_B": total_a / total_b if total_b else None}
