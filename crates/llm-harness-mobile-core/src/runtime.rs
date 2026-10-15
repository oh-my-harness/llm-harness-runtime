//! Process-wide tokio runtime management.
//!
//! The runtime is created lazily and lives for the whole process. All FFI
//! entry points drive async work through [`run_on_runtime`].

/// Install a global multi-thread tokio runtime exactly once.
///
/// FFI entry points that need async work call `runtime.block_on(...)`, which
/// enters the runtime context for the duration — that is what gives
/// `tokio::fs`/`tokio::spawn`/reqwest their reactor. Do NOT `enter()` here:
/// foreign threads call these repeatedly and nested EnterGuards must be
/// dropped in reverse order (tokio panics otherwise).
/// Run `future` to completion on the process-wide runtime.
///
/// This is the ONLY place allowed to call `block_on`. Calls that originate
/// from inside a runtime-driven task (event pump, spawned prompt) must not
/// nest `block_on` — tokio panics on that. Foreign FFI threads have no
/// ambient context and get `block_on`; runtime threads get the future
/// spawned onto the runtime instead.
pub(crate) fn run_on_runtime<F>(future: F) -> F::Output
where
    F: std::future::Future,
{
    let runtime = ensure_runtime();
    match tokio::runtime::Handle::try_current() {
        // Already on a runtime thread: cannot block that thread (it may be
        // driving other tasks). LocalSet-style await is unavailable in a sync
        // fn, so detach a task instead — Send bound applies to spawn only.
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(future)),
        Err(_) => runtime.block_on(future),
    }
}

pub(crate) fn ensure_runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime")
    })
}
