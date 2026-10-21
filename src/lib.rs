#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(missing_docs, missing_debug_implementations, rust_2018_idioms)]

#[cfg(all(target_family = "wasm", target_feature = "atomics"))]
compile_error!(
    "tower-worker-fetch assumes single-threaded wasm without the `atomics` target feature: \
     its JavaScript futures and handles are marked `Send + Sync` on that basis, which does not \
     hold when `atomics` is enabled"
);

mod abort;
mod body;
mod error;
mod service;

pub use body::Body;
pub use error::{Error, Stage};
pub use service::{FetchService, Response, ResponseFuture};

/// Request extension types honoured by [`FetchService`], re-exported from
/// [`worker`] so callers do not need a direct `worker` dependency for the
/// common case.
///
/// Insert a [`worker::AbortSignal`] into `request.extensions_mut()` to make
/// the fetch cancellable:
///
/// ```rust,no_run
/// use tower_worker_fetch::{AbortController, FetchService};
/// use tower_service::Service as _;
///
/// # async fn example(mut svc: FetchService) -> Result<(), tower_worker_fetch::Error> {
/// let controller = AbortController::default();
/// let mut req = http::Request::get("https://example.com/")
///     .body(http_body_util::Empty::<bytes::Bytes>::new())
///     .unwrap();
/// req.extensions_mut().insert(controller.signal());
///
/// let fut = svc.call(req);
/// controller.abort(); // the future now resolves to an error with `is_abort() == true`
/// let err = fut.await.unwrap_err();
/// assert!(err.is_abort());
/// # Ok(())
/// # }
/// ```
pub use worker::{AbortController, AbortSignal};
