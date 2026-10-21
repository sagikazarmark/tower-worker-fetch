//! The service.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use pin_project_lite::pin_project;
use tower_service::Service;
use worker::send::SendFuture;
use worker::wasm_bindgen::{JsCast, JsValue};
use worker::wasm_bindgen_futures::JsFuture;
use worker::{js_sys, worker_sys};

use crate::abort::{self, AbortGuard};
use crate::body::Body;
use crate::error::{Error, Stage};

/// The response type of [`FetchService`]: an `http::Response` with a
/// streaming [`Body`].
pub type Response = http::Response<Body>;

/// A [`tower_service::Service`] that performs a Cloudflare Workers `fetch`.
///
/// Accepts `http::Request<B>` for any `B: http_body::Body<Data = Bytes> + 'static`
/// and resolves to [`Response`]. Targets either the global `fetch` function
/// ([`FetchService::new`]) or a service binding ([`From<worker::Fetcher>`]).
///
/// The service is `Clone`, `Send` and `Sync`, and so is its future, so it
/// composes with `tower::ServiceBuilder`, `tower::util::BoxCloneService` and
/// `BoxCloneSyncService`. See the crate documentation for what that claim
/// rests on.
///
/// `poll_ready` is always ready: Workers has no connection pool to apply
/// backpressure against.
#[derive(Clone, Debug, Default)]
pub struct FetchService {
    target: Target,
}

/// `Send + Sync` by auto-trait: `worker_sys::Fetcher` wraps a `JsValue`, and
/// wasm-bindgen marks `JsValue` `Send + Sync` on targets without the `atomics`
/// feature (every Workers build).
#[derive(Clone, Debug, Default)]
enum Target {
    #[default]
    Global,
    Fetcher(worker_sys::Fetcher),
}

impl FetchService {
    /// A service that calls the global `fetch`.
    pub fn new() -> Self {
        Self::default()
    }
}

impl From<worker::Fetcher> for FetchService {
    /// A service that calls `fetch` on a service binding, as returned by
    /// `env.service("NAME")`.
    fn from(fetcher: worker::Fetcher) -> Self {
        // `worker::Fetcher` only exposes `fetch_request`, which converts,
        // fetches and converts back inside one `Result`. The `worker_sys`
        // handle underneath gives us `fetch(request)` directly.
        let fetcher: worker_sys::Fetcher = JsValue::from(fetcher).unchecked_into();
        Self {
            target: Target::Fetcher(fetcher),
        }
    }
}

impl<B> Service<http::Request<B>> for FetchService
where
    B: http_body::Body<Data = Bytes> + 'static,
{
    type Response = Response;
    type Error = Error;
    type Future = ResponseFuture;

    /// Always ready. Workers has no connection pool, so there is nothing to
    /// wait for; do not "fix" this.
    #[inline]
    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        let target = self.target.clone();
        ResponseFuture::new(fetch(target, req))
    }
}

async fn fetch<B>(target: Target, mut req: http::Request<B>) -> Result<Response, Error>
where
    B: http_body::Body<Data = Bytes> + 'static,
{
    // Lives in this future's state until the response arrives, then moves
    // into the body: dropping either before completion aborts the fetch.
    let mut guard = AbortGuard::new();
    abort::apply_timeout(req.extensions_mut());
    abort::add_signal(req.extensions_mut(), guard.signal());

    // Removes `AbortSignal` from the extensions and sets it on the Request;
    // streams the body (see the crate docs on request bodies).
    let request: web_sys::Request =
        worker::request_to_wasm(req).map_err(|e| Error::from_worker(Stage::Request, e))?;

    // No `RequestInit` on the fetch call itself, so the Request's own
    // `signal` is the one in effect.
    let promise = match &target {
        Target::Global => global_scope().fetch_with_request(&request),
        Target::Fetcher(fetcher) => fetcher
            .fetch(&request)
            .map_err(|e| Error::from_js(Stage::Fetch, e))?,
    };

    let result = JsFuture::from(promise).await;
    let value = match result {
        Ok(value) => value,
        Err(e) => {
            // The fetch is already over; aborting now would be noise.
            guard.disarm();
            return Err(Error::from_js(Stage::Fetch, e));
        }
    };
    let response: web_sys::Response = value.dyn_into().map_err(|_| {
        Error::from_message(
            Stage::Fetch,
            "fetch resolved to a value that is not a Response",
        )
    })?;

    let response =
        worker::response_from_wasm(response).map_err(|e| Error::from_worker(Stage::Response, e))?;

    Ok(response.map(|body| Body::new(body, Some(guard))))
}

fn global_scope() -> web_sys::WorkerGlobalScope {
    js_sys::global().unchecked_into()
}

type BoxedResponseFuture = Pin<Box<dyn Future<Output = Result<Response, Error>>>>;

pin_project! {
    /// The future returned by [`FetchService::call`].
    ///
    /// Dropping it before it resolves aborts the fetch (the subrequest is
    /// cancelled, as with reqwest). Once it has resolved, the response
    /// [`Body`] takes over that role; see its documentation.
    ///
    /// `Send + Sync` via [`worker::send::SendFuture`]: the boxed future inside
    /// holds a `JsFuture` (`Rc`-based, `!Send`), the per-fetch
    /// `AbortController` handle, and the caller's request body until the
    /// request has been converted. See the crate documentation.
    pub struct ResponseFuture {
        #[pin]
        inner: SendFuture<BoxedResponseFuture>,
    }
}

impl ResponseFuture {
    fn new<F>(future: F) -> Self
    where
        F: Future<Output = Result<Response, Error>> + 'static,
    {
        Self {
            inner: SendFuture::new(Box::pin(future)),
        }
    }
}

impl Future for ResponseFuture {
    type Output = Result<Response, Error>;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().inner.poll(cx)
    }
}

impl fmt::Debug for ResponseFuture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResponseFuture").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync_static<T: Send + Sync + 'static>() {}

    #[test]
    fn service_and_future_are_send_sync_static() {
        assert_send_sync_static::<FetchService>();
        assert_send_sync_static::<ResponseFuture>();
    }

    #[test]
    fn service_is_clone_and_default() {
        let svc = FetchService::new();
        let _clone = svc.clone();
        let _default = FetchService::default();
    }

    #[test]
    fn poll_ready_is_always_ready() {
        let mut svc = FetchService::new();
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        let ready =
            <FetchService as Service<http::Request<http_body_util::Empty<Bytes>>>>::poll_ready(
                &mut svc, &mut cx,
            );
        assert!(matches!(ready, Poll::Ready(Ok(()))));
    }

    #[test]
    fn satisfies_tower_box_clone_service_bounds() {
        // Mirrors `tower::util::BoxCloneService::new`'s bounds without
        // depending on `tower`.
        fn assert<S, R>()
        where
            S: Service<R> + Clone + Send + 'static,
            S::Future: Send + 'static,
        {
        }
        assert::<FetchService, http::Request<http_body_util::Full<Bytes>>>();
    }
}
