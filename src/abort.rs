//! Cancel-on-drop plumbing: an `AbortController` per fetch, and the step that
//! merges its signal with whatever `AbortSignal` the caller supplied.

use std::fmt;

use worker::js_sys;
use worker::wasm_bindgen::JsValue;

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
}
