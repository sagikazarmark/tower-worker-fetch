//! Cancellation plumbing: the [`Timeout`] request extension, an
//! `AbortController` per fetch, and the step that merges their signals with
//! whatever `AbortSignal` the caller supplied.

use std::fmt;
use std::time::Duration;

use worker::js_sys;
use worker::wasm_bindgen::JsValue;

/// Request extension: abort the fetch if it has not finished within the
/// duration.
///
/// Insert it into `request.extensions_mut()`. The service turns it into an
/// `AbortSignal.timeout()` on the outgoing request, combined with your
/// [`AbortSignal`] extension (if any) via `AbortSignal.any`, so either one
/// cancels the fetch. No `web-sys` dependency is needed on your side.
///
/// ```rust,no_run
/// use std::time::Duration;
/// use tower_worker_fetch::Timeout;
///
/// # fn example(req: &mut http::Request<()>) {
/// req.extensions_mut().insert(Timeout(Duration::from_secs(10)));
/// # }
/// ```
///
/// **The timeout spans the whole exchange, including reading the body.** That
/// is how `AbortSignal.timeout()` works: the timer is not stopped when the
/// headers arrive. A timeout that fires before the response future resolves
/// produces an [`Error`] with `stage() == Stage::Fetch`; one that fires while
/// the [`Body`] is still streaming surfaces from the body with
/// `stage() == Stage::Body`. Either way `is_timeout()` and `is_abort()` are
/// both `true` (the runtime aborts with a `TimeoutError`).
///
/// The duration is rounded up to whole milliseconds, saturating at `u32::MAX`.
/// The runtime's `setTimeout` caps delays at about 24.8 days regardless.
///
/// [`AbortSignal`]: crate::AbortSignal
/// [`Error`]: crate::Error
/// [`Body`]: crate::Body
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timeout(pub Duration);

impl Timeout {
    /// Whole milliseconds, rounded up and saturated to `u32::MAX`: the
    /// argument to `AbortSignal.timeout()`. Rounding up keeps a
    /// sub-millisecond timeout from becoming `timeout(0)`, which would fire
    /// before the fetch has had a chance to run.
    pub(crate) fn millis(self) -> u32 {
        u32::try_from(self.0.as_nanos().div_ceil(1_000_000)).unwrap_or(u32::MAX)
    }
}

/// If the request carries a [`Timeout`] extension, remove it and fold the
/// matching `AbortSignal.timeout()` into the request's cancellation (see
/// [`add_signal`]).
pub(crate) fn apply_timeout(extensions: &mut http::Extensions) {
    if let Some(timeout) = extensions.remove::<Timeout>() {
        let signal = web_sys::AbortSignal::timeout_with_u32(timeout.millis());
        add_signal(extensions, signal);
    }
}

/// Aborts a `web_sys::AbortController` when dropped, unless disarmed.
///
/// One is created per fetch. It travels inside the in-flight future (so
/// dropping the future aborts the request), then into the response [`Body`]
/// (so dropping a half-read body cancels the `ReadableStream`), and is
/// disarmed once there is nothing left to cancel.
///
/// `Send + Sync` by auto-trait: `web_sys::AbortController` wraps a `JsValue`,
/// which wasm-bindgen marks `Send + Sync` on targets without the `atomics`
/// feature. Nothing ever observes the internal abort as an error: by the time
/// the guard fires, no Rust code is polling the fetch or the stream.
///
/// [`Body`]: crate::Body
pub(crate) struct AbortGuard {
    controller: Option<web_sys::AbortController>,
}

impl AbortGuard {
    /// A fresh controller, armed.
    pub(crate) fn new() -> Self {
        Self {
            controller: Some(
                web_sys::AbortController::new().expect("AbortController constructor cannot throw"),
            ),
        }
    }

    /// The signal to attach to the request.
    pub(crate) fn signal(&self) -> web_sys::AbortSignal {
        self.controller
            .as_ref()
            .expect("an armed guard has a controller")
            .signal()
    }

    /// Drop without aborting: the operation finished (or was handed to the
    /// caller as a raw handle) and must not be cancelled from here on.
    pub(crate) fn disarm(&mut self) {
        self.controller = None;
    }
}

impl Drop for AbortGuard {
    fn drop(&mut self) {
        if let Some(controller) = self.controller.take() {
            controller.abort();
        }
    }
}

impl fmt::Debug for AbortGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AbortGuard")
            .field("armed", &self.controller.is_some())
            .finish()
    }
}

/// Add `signal` to the request's cancellation: combine it with the caller's
/// `AbortSignal` extension if there is one (`AbortSignal.any`), and leave the
/// result in the extensions as the single `worker::AbortSignal` that
/// `worker::request_to_wasm` sets on the outgoing request.
///
/// Caller-side aborts keep working exactly as before: an `AbortSignal.any`
/// signal aborts with the reason of whichever source fired first.
pub(crate) fn add_signal(extensions: &mut http::Extensions, signal: web_sys::AbortSignal) {
    let combined = match extensions.remove::<worker::AbortSignal>() {
        Some(caller) => {
            let caller: &JsValue = caller.as_ref();
            let signals = js_sys::Array::of2(caller, signal.as_ref());
            web_sys::AbortSignal::any(&signals)
        }
        None => signal,
    };
    extensions.insert(worker::AbortSignal::from(combined));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_is_send_sync_static() {
        fn assert<T: Send + Sync + 'static>() {}
        assert::<AbortGuard>();
    }

    #[test]
    fn disarmed_guard_drops_quietly() {
        // No JS runtime on the host: a guard with no controller must not
        // touch one on drop.
        let mut guard = AbortGuard { controller: None };
        guard.disarm();
        drop(guard);
    }

    #[test]
    fn timeout_is_send_sync_copy_static() {
        fn assert<T: Send + Sync + Copy + 'static>() {}
        assert::<Timeout>();
    }

    #[test]
    fn timeout_is_a_plain_value() {
        // Constructible and comparable on the host, no wasm-bindgen involved.
        let timeout = Timeout(Duration::from_secs(10));
        let copy = timeout;
        assert_eq!(timeout, copy);
        assert_eq!(timeout.0, Duration::from_secs(10));
        let mut extensions = http::Extensions::new();
        extensions.insert(timeout);
        assert_eq!(extensions.get::<Timeout>(), Some(&timeout));
    }

    #[test]
    fn timeout_millis_round_up_and_saturate() {
        assert_eq!(Timeout(Duration::ZERO).millis(), 0);
        assert_eq!(Timeout(Duration::from_millis(1500)).millis(), 1500);
        assert_eq!(Timeout(Duration::from_micros(500)).millis(), 1);
        assert_eq!(Timeout(Duration::from_micros(1999)).millis(), 2);
        assert_eq!(Timeout(Duration::from_micros(2000)).millis(), 2);
        assert_eq!(
            Timeout(Duration::from_millis(u64::from(u32::MAX))).millis(),
            u32::MAX
        );
        assert_eq!(
            Timeout(Duration::from_millis(u64::from(u32::MAX) + 1)).millis(),
            u32::MAX
        );
        assert_eq!(Timeout(Duration::MAX).millis(), u32::MAX);
    }

    #[test]
    fn apply_timeout_without_extension_is_a_no_op() {
        // No JS runtime on the host: with no `Timeout` present nothing may
        // reach `AbortSignal.timeout()`.
        let mut extensions = http::Extensions::new();
        extensions.insert(42u8);
        apply_timeout(&mut extensions);
        assert_eq!(extensions.get::<u8>(), Some(&42));
    }
}
