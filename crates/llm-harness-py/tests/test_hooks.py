"""Tests for hook context converters and hook creation."""

import json

import llm_harness_py


# ── Module surface tests ────────────────────────────────────────────────────


def test_module_has_create_before_turn_hook():
    """The module exposes create_before_turn_hook."""
    assert hasattr(llm_harness_py, "create_before_turn_hook")


def test_module_has_hook_class():
    """The module exposes a Hook class."""
    assert hasattr(llm_harness_py, "Hook")


# ── create_before_turn_hook tests ───────────────────────────────────────────


def test_create_before_turn_hook():
    """create_before_turn_hook returns a non-None Hook object."""

    def my_hook(ctx):
        pass

    hook = llm_harness_py.create_before_turn_hook(my_hook)
    assert hook is not None


def test_create_before_turn_hook_with_lambda():
    """create_before_turn_hook accepts a lambda callback."""
    hook = llm_harness_py.create_before_turn_hook(lambda ctx: None)
    assert hook is not None


def test_create_before_turn_hook_returns_hook_instance():
    """The returned object is an instance of the Hook class."""
    hook = llm_harness_py.create_before_turn_hook(lambda ctx: None)
    assert isinstance(hook, llm_harness_py.Hook)


def test_multiple_before_turn_hooks_independent():
    """Multiple hook instances are independent."""
    h1 = llm_harness_py.create_before_turn_hook(lambda ctx: None)
    h2 = llm_harness_py.create_before_turn_hook(lambda ctx: None)
    assert h1 is not h2


def test_before_turn_hook_accepts_async_callback():
    """create_before_turn_hook accepts an async def callback."""

    async def my_async_hook(ctx):
        pass

    hook = llm_harness_py.create_before_turn_hook(my_async_hook)
    assert hook is not None


# ── Hook callback invocation tests (via Rust integration) ───────────────────
# The following tests verify that the Python callback is actually invoked when
# the BeforeTurnHook trait method is called. Full end-to-end invocation is
# tested in the Rust integration test (tests/hooks_integration.rs) because it
# requires tokio + Python::attach.


# ── create_after_turn_hook tests ─────────────────────────────────────────────


def test_create_after_turn_hook():
    """create_after_turn_hook returns a non-None Hook object."""

    def my_hook(ctx):
        pass

    hook = llm_harness_py.create_after_turn_hook(my_hook)
    assert hook is not None


def test_create_after_turn_hook_returns_hook_instance():
    """The returned object is an instance of the Hook class."""
    hook = llm_harness_py.create_after_turn_hook(lambda ctx: None)
    assert isinstance(hook, llm_harness_py.Hook)


def test_create_after_turn_hook_accepts_async_callback():
    """create_after_turn_hook accepts an async def callback."""

    async def my_async_hook(ctx):
        pass

    hook = llm_harness_py.create_after_turn_hook(my_async_hook)
    assert hook is not None


# ── create_before_run_hook tests ─────────────────────────────────────────────


def test_create_before_run_hook():
    """create_before_run_hook returns a non-None Hook object."""

    def my_hook(ctx):
        pass

    hook = llm_harness_py.create_before_run_hook(my_hook)
    assert hook is not None


def test_create_before_run_hook_returns_hook_instance():
    """The returned object is an instance of the Hook class."""
    hook = llm_harness_py.create_before_run_hook(lambda ctx: None)
    assert isinstance(hook, llm_harness_py.Hook)


def test_create_before_run_hook_accepts_async_callback():
    """create_before_run_hook accepts an async def callback."""

    async def my_async_hook(ctx):
        pass

    hook = llm_harness_py.create_before_run_hook(my_async_hook)
    assert hook is not None


# ── create_after_provider_response_hook tests ────────────────────────────────


def test_create_after_provider_response_hook():
    """create_after_provider_response_hook returns a non-None Hook object."""

    def my_hook(info):
        pass

    hook = llm_harness_py.create_after_provider_response_hook(my_hook)
    assert hook is not None


def test_create_after_provider_response_hook_returns_hook_instance():
    """The returned object is an instance of the Hook class."""
    hook = llm_harness_py.create_after_provider_response_hook(lambda info: None)
    assert isinstance(hook, llm_harness_py.Hook)


def test_create_after_provider_response_hook_accepts_async_callback():
    """create_after_provider_response_hook accepts an async def callback."""

    async def my_async_hook(info):
        pass

    hook = llm_harness_py.create_after_provider_response_hook(my_async_hook)
    assert hook is not None


# ── create_before_provider_request_hook tests ────────────────────────────────


def test_create_before_provider_request_hook():
    """create_before_provider_request_hook returns a non-None Hook object."""

    def my_hook(opts):
        pass

    hook = llm_harness_py.create_before_provider_request_hook(my_hook)
    assert hook is not None


def test_create_before_provider_request_hook_returns_hook_instance():
    """The returned object is an instance of the Hook class."""
    hook = llm_harness_py.create_before_provider_request_hook(lambda opts: None)
    assert isinstance(hook, llm_harness_py.Hook)


def test_create_before_provider_request_hook_accepts_async_callback():
    """create_before_provider_request_hook accepts an async def callback."""

    async def my_async_hook(opts):
        pass

    hook = llm_harness_py.create_before_provider_request_hook(my_async_hook)
    assert hook is not None



# ── create_should_stop_hook tests ───────────────────────────────────────────


def test_create_should_stop_hook():
    """create_should_stop_hook returns a non-None Hook object."""

    def my_hook(ctx):
        return True

    hook = llm_harness_py.create_should_stop_hook(my_hook)
    assert hook is not None


def test_create_should_stop_hook_returns_hook_instance():
    """The returned object is an instance of the Hook class."""
    hook = llm_harness_py.create_should_stop_hook(lambda ctx: True)
    assert isinstance(hook, llm_harness_py.Hook)


def test_create_should_stop_hook_accepts_async_callback():
    """create_should_stop_hook accepts an async def callback."""

    async def my_async_hook(ctx):
        return True

    hook = llm_harness_py.create_should_stop_hook(my_async_hook)
    assert hook is not None


# ── create_before_tool_call_hook tests ──────────────────────────────────────


def test_create_before_tool_call_hook():
    """create_before_tool_call_hook returns a non-None Hook object."""

    def my_hook(ctx):
        return "allow"

    hook = llm_harness_py.create_before_tool_call_hook(my_hook)
    assert hook is not None


def test_create_before_tool_call_hook_returns_hook_instance():
    """The returned object is an instance of the Hook class."""
    hook = llm_harness_py.create_before_tool_call_hook(lambda ctx: "allow")
    assert isinstance(hook, llm_harness_py.Hook)


def test_create_before_tool_call_hook_accepts_async_callback():
    """create_before_tool_call_hook accepts an async def callback."""

    async def my_async_hook(ctx):
        return "allow"

    hook = llm_harness_py.create_before_tool_call_hook(my_async_hook)
    assert hook is not None


def test_create_before_tool_call_hook_accepts_dict_return():
    """create_before_tool_call_hook accepts a dict with action=modify."""

    def my_hook(ctx):
        return {"action": "modify", "args": {"path": "/modified"}}

    hook = llm_harness_py.create_before_tool_call_hook(my_hook)
    assert hook is not None


# ── create_after_tool_call_hook tests ───────────────────────────────────────


def test_create_after_tool_call_hook():
    """create_after_tool_call_hook returns a non-None Hook object."""

    def my_hook(ctx):
        return "passthrough"

    hook = llm_harness_py.create_after_tool_call_hook(my_hook)
    assert hook is not None


def test_create_after_tool_call_hook_returns_hook_instance():
    """The returned object is an instance of the Hook class."""
    hook = llm_harness_py.create_after_tool_call_hook(lambda ctx: "passthrough")
    assert isinstance(hook, llm_harness_py.Hook)


def test_create_after_tool_call_hook_accepts_async_callback():
    """create_after_tool_call_hook accepts an async def callback."""

    async def my_async_hook(ctx):
        return "passthrough"

    hook = llm_harness_py.create_after_tool_call_hook(my_async_hook)
    assert hook is not None


# ── create_before_compact_hook tests ────────────────────────────────────────


def test_create_before_compact_hook():
    """create_before_compact_hook returns a non-None Hook object."""

    def my_hook(ctx):
        return "proceed"

    hook = llm_harness_py.create_before_compact_hook(my_hook)
    assert hook is not None


def test_create_before_compact_hook_returns_hook_instance():
    """The returned object is an instance of the Hook class."""
    hook = llm_harness_py.create_before_compact_hook(lambda ctx: "proceed")
    assert isinstance(hook, llm_harness_py.Hook)


def test_create_before_compact_hook_accepts_async_callback():
    """create_before_compact_hook accepts an async def callback."""

    async def my_async_hook(ctx):
        return "proceed"

    hook = llm_harness_py.create_before_compact_hook(my_async_hook)
    assert hook is not None


# ── create_transform_context_hook tests ──────────────────────────────────────


def test_create_transform_context_hook():
    """create_transform_context_hook returns a non-None Hook object."""

    def my_hook(ctx):
        return ctx

    hook = llm_harness_py.create_transform_context_hook(my_hook)
    assert hook is not None


def test_create_transform_context_hook_returns_hook_instance():
    """The returned object is an instance of the Hook class."""
    hook = llm_harness_py.create_transform_context_hook(lambda ctx: ctx)
    assert isinstance(hook, llm_harness_py.Hook)


def test_create_transform_context_hook_accepts_async_callback():
    """create_transform_context_hook accepts an async def callback."""

    async def my_async_hook(ctx):
        return ctx

    hook = llm_harness_py.create_transform_context_hook(my_async_hook)
    assert hook is not None


# ── create_prepare_next_turn_hook tests ──────────────────────────────────────


def test_create_prepare_next_turn_hook():
    """create_prepare_next_turn_hook returns a non-None Hook object."""

    def my_hook(ctx):
        return {}

    hook = llm_harness_py.create_prepare_next_turn_hook(my_hook)
    assert hook is not None


def test_create_prepare_next_turn_hook_returns_hook_instance():
    """The returned object is an instance of the Hook class."""
    hook = llm_harness_py.create_prepare_next_turn_hook(lambda ctx: {})
    assert isinstance(hook, llm_harness_py.Hook)


def test_create_prepare_next_turn_hook_accepts_async_callback():
    """create_prepare_next_turn_hook accepts an async def callback."""

    async def my_async_hook(ctx):
        return {}

    hook = llm_harness_py.create_prepare_next_turn_hook(my_async_hook)
    assert hook is not None