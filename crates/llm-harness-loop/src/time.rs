//! Target-agnostic time primitives for the agent loop.
//!
//! Native: thin re-exports of `tokio::time` (`sleep`, `timeout`) plus
//! `SleepExt::reset_for`. wasm32: `tokio` has no time driver on
//! wasm32-unknown-unknown (`Instant::now` panics — probed 2026-09-04, see
//! issue #180), so this module bridges to `futures_timer::Delay`, which
//! drives `gloo_timers::TimeoutFuture` (setTimeout) there.
//!
//! The wasm `Delay` implements `reset` by discarding the old timer and
//! creating a new one, matching the hoisted-Sleep + re-arm pattern the
//! idle watchdog uses — no structural fork at the call sites. The watchdog
//! deadline is therefore expressed as a `Duration` from "now" on both
//! targets (`SleepExt::reset_for`), never as an absolute `Instant`.

use std::pin::Pin;

// Native: tokio primitives as-is.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use tokio::time::{sleep, timeout};

// wasm32: futures-timer equivalents. `Delay` is a drop-in `Sleep`:
// `new(Duration)` + `reset(Duration)`, both "fire after this long".
#[cfg(target_arch = "wasm32")]
pub(crate) fn sleep(dur: std::time::Duration) -> futures_timer::Delay {
    futures_timer::Delay::new(dur)
}

/// `tokio::time::timeout` equivalent: complete `fut` before `dur` elapses.
///
/// Returns `Ok(output)` on success, `Err(())` on deadline (tokio returns
/// `Err(Elapsed)`; the loop only branches on its existence, so a unit error
/// keeps the callsite shape identical).
#[cfg(target_arch = "wasm32")]
pub(crate) async fn timeout<F: Future>(dur: std::time::Duration, fut: F) -> Result<F::Output, ()> {
    use futures::FutureExt;
    futures::select_biased! {
        output = fut.fuse() => Ok(output),
        _ = sleep(dur).fuse() => Err(()),
    }
}

/// Tokio's `Sleep::reset` takes an absolute `Instant`; futures-timer's
/// `Delay::reset` takes a relative `Duration`. The watchdog only needs
/// "re-arm for another N ms", so the difference is absorbed here.
pub(crate) trait SleepExt {
    fn reset_for(self: Pin<&mut Self>, dur: std::time::Duration);
}

#[cfg(not(target_arch = "wasm32"))]
impl SleepExt for tokio::time::Sleep {
    fn reset_for(self: Pin<&mut Self>, dur: std::time::Duration) {
        tokio::time::Sleep::reset(self, tokio::time::Instant::now() + dur);
    }
}

#[cfg(target_arch = "wasm32")]
impl SleepExt for futures_timer::Delay {
    fn reset_for(self: Pin<&mut Self>, dur: std::time::Duration) {
        futures_timer::Delay::reset(self.get_mut(), dur);
    }
}
