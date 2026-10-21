//! Smoke test Worker for `tower-worker-fetch`.
//!
//! Runs under `wrangler dev` and fetches its own origin, so it only makes sense
//! locally or in CI. Every `/test-*` route returns `200 {"ok":true,...}` on
//! success and `500 {"ok":false,"error":...}` on failure. `run.sh` drives it.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use base64::Engine as _;
use bytes::Bytes;
use futures_util::future::{select, Either};
use futures_util::StreamExt as _;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Empty, Full};
use serde::{Deserialize, Serialize};
use tower::util::BoxCloneService;
use tower::{Service as _, ServiceBuilder, ServiceExt as _};
use tower_worker_fetch::{AbortSignal, Body, Error, FetchService, Stage};
use worker::js_sys::{Function, Promise, Reflect};
use worker::wasm_bindgen::closure::Closure;
use worker::wasm_bindgen::{JsCast as _, JsValue};
use worker::{event, Context, Delay, Env, HttpRequest, Result};

type BoxError = Box<dyn std::error::Error + Send + Sync>;
/// What every `/test-*` route produces: the JSON to report, or why it failed.
type TestResult = std::result::Result<serde_json::Value, BoxError>;
/// Common response body for every route: fixed bytes or a `worker::Body` stream.
type SmokeBody = UnsyncBoxBody<Bytes, worker::Error>;
type SmokeResponse = http::Response<SmokeBody>;

#[event(fetch)]
async fn fetch(req: HttpRequest, env: Env, _ctx: Context) -> Result<SmokeResponse> {
    let origin = origin_of(&req)?;
    let path = req.uri().path().to_owned();

    match path.as_str() {
        "/echo" => echo(req).await,
        "/echo-stream" => echo_stream(&req),
        "/slow" => slow(&req).await,

        "/test-get" => report(test_get(&origin).await),
        "/test-post" => report(test_post(&origin).await),
        "/test-stream" => report(test_stream(&origin).await),
        "/test-abort" => report(test_abort(&origin).await),
        "/test-timeout" => report(test_timeout(&origin).await),
        "/test-drop-future" => report(test_drop_future(&origin, &env).await),
        "/test-drop-body" => report(test_drop_body(&origin).await),
        "/test-binding" => report(test_binding(&origin, &env).await),
        "/test-tower" => report(test_tower(&origin).await),

        _ => text(http::StatusCode::NOT_FOUND, "not found"),
    }
}

// ---------------------------------------------------------------------------
// Helper routes

#[derive(Serialize, Deserialize)]
struct Echo {
    method: String,
    headers: BTreeMap<String, String>,
    body_len: usize,
    body_b64: String,
}

async fn echo(req: HttpRequest) -> Result<SmokeResponse> {
    let method = req.method().to_string();
    let headers = req
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_owned(),
                v.to_str().unwrap_or("<binary>").to_owned(),
            )
        })
        .collect();
    let body = BodyExt::collect(req.into_body()).await?.to_bytes();
    json(
        http::StatusCode::OK,
        &Echo {
            method,
            headers,
            body_len: body.len(),
            body_b64: base64::engine::general_purpose::STANDARD.encode(&body),
        },
    )
}

fn stream_chunk(i: u32) -> Vec<u8> {
    format!("chunk-{i};").into_bytes()
}

fn echo_stream(req: &HttpRequest) -> Result<SmokeResponse> {
    let n: u32 = query_param(req, "n").unwrap_or(5);
    let stream = futures_util::stream::iter(0..n).then(|i| async move {
        Delay::from(Duration::from_millis(25)).await;
        Ok::<_, worker::Error>(stream_chunk(i))
    });
    Ok(http::Response::builder()
        .status(http::StatusCode::OK)
        .header("content-type", "application/octet-stream")
        .body(worker::Body::from_stream(stream)?.boxed_unsync())?)
}

async fn slow(req: &HttpRequest) -> Result<SmokeResponse> {
    let ms: u64 = query_param(req, "ms").unwrap_or(2000);
    Delay::from(Duration::from_millis(ms)).await;
    SLOW_COMPLETED.fetch_add(1, Ordering::Relaxed);
    text(http::StatusCode::OK, "slow")
}

/// Bumped by `/slow` once its delay is over. The smoke Worker fetches itself
/// inside one isolate under `wrangler dev`, so a test route can read what the
/// handler it called managed to do. Only deltas are meaningful.
static SLOW_COMPLETED: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// Observing the outgoing fetch
//
// Cancel-on-drop is internal to `FetchService`: nothing on the Rust side can
// see its `AbortController`. The test routes below observe it from the JS
// side instead, by wrapping `globalThis.fetch` so the `signal` of the most
// recent outgoing Request is kept around to inspect after the drop.
//
// Server-side observation was tried first and found unreliable for bodies
// under local workerd: once a response has been returned to the caller,
// aborting (or even `ReadableStream.cancel()`-ing) the response body does not
// reach the handler that produced it, whether through global fetch or a
// service binding. For the in-flight case it does work through a service
// binding (the handler is cancelled with its caller), so `/test-drop-future`
// checks both.

thread_local! {
    static LAST_FETCH_SIGNAL: RefCell<Option<web_sys::AbortSignal>> = const { RefCell::new(None) };
}

/// Install the `globalThis.fetch` wrapper, once per isolate.
fn observe_fetch() {
    thread_local! {
        static INSTALLED: RefCell<bool> = const { RefCell::new(false) };
    }
    if INSTALLED.replace(true) {
        return;
    }

    let global = worker::js_sys::global();
    let original: Function = Reflect::get(&global, &JsValue::from_str("fetch"))
        .expect("globalThis.fetch")
        .unchecked_into();
    let this = global.clone();
    let wrapper = Closure::<dyn FnMut(JsValue, JsValue) -> Promise>::new(
        move |request: JsValue, init: JsValue| {
            let signal = request
                .dyn_ref::<web_sys::Request>()
                .map(web_sys::Request::signal);
            LAST_FETCH_SIGNAL.set(signal);
            original
                .call2(&this, &request, &init)
                .expect("fetch does not throw synchronously")
                .unchecked_into()
        },
    );
    Reflect::set(&global, &JsValue::from_str("fetch"), wrapper.as_ref())
        .expect("globalThis.fetch is writable");
    wrapper.forget();
}

/// The `signal` of the most recent outgoing global fetch.
fn last_fetch_signal() -> std::result::Result<web_sys::AbortSignal, BoxError> {
    LAST_FETCH_SIGNAL
        .with_borrow(Clone::clone)
        .ok_or_else(|| "no outgoing fetch was observed".into())
}

// ---------------------------------------------------------------------------
// Test routes

async fn test_get(origin: &str) -> TestResult {
    echo_header_round_trip(FetchService::new(), origin, "get").await
}

async fn test_post(origin: &str) -> TestResult {
    const PAYLOAD: &[u8] = b"hello from tower-worker-fetch";
    let req = http::Request::post(format!("{origin}/echo"))
        .header("content-type", "text/plain")
        .body(Full::new(Bytes::from_static(PAYLOAD)))?;

    let resp = FetchService::new().oneshot(req).await?;
    ensure(resp.status() == http::StatusCode::OK, "status 200")?;
    let echo: Echo = read_json(resp.into_body()).await?;
    ensure(echo.method == "POST", "method is POST")?;
    let body = base64::engine::general_purpose::STANDARD.decode(&echo.body_b64)?;
    ensure(body == PAYLOAD, "body round-trips")?;
    // Not asserted, recorded: what the runtime put on the wire for a streamed `Full<Bytes>`.
    Ok(serde_json::json!({
        "ok": true,
        "content_length": echo.headers.get("content-length"),
        "transfer_encoding": echo.headers.get("transfer-encoding"),
    }))
}

async fn test_stream(origin: &str) -> TestResult {
    const N: u32 = 5;
    let expected_len: usize = (0..N).map(|i| stream_chunk(i).len()).sum();
    let req =
        http::Request::get(format!("{origin}/echo-stream?n={N}")).body(Empty::<Bytes>::new())?;

    let resp = FetchService::new().oneshot(req).await?;
    ensure(resp.status() == http::StatusCode::OK, "status 200")?;

    let mut body = resp.into_body();
    let mut frames = 0usize;
    let mut bytes = 0usize;
    while let Some(frame) = body.frame().await {
        let frame = frame?;
        if let Some(data) = frame.data_ref() {
            frames += 1;
            bytes += data.len();
        }
    }
    ensure(bytes == expected_len, "total bytes match")?;
    ensure(frames >= 2, "response arrived in more than one frame")?;
    Ok(serde_json::json!({ "ok": true, "frames": frames, "bytes": bytes }))
}

async fn test_abort(origin: &str) -> TestResult {
    expect_cancelled(origin, AbortSignal::abort(), Cancellation::Abort).await
}

async fn test_timeout(origin: &str) -> TestResult {
    let signal = AbortSignal::from(web_sys::AbortSignal::timeout_with_u32(50));
    expect_cancelled(origin, signal, Cancellation::Timeout).await
}

/// Dropping the response future mid-flight aborts the fetch: the outgoing
/// Request's signal fires, and (through a service binding, where local
/// workerd propagates it) the `/slow` handler is cancelled before it can
/// bump its counter. A fetch that is awaited does neither.
async fn test_drop_future(origin: &str, env: &Env) -> TestResult {
    const SLOW_MS: u64 = 300;
    observe_fetch();
    let slow_req =
        || http::Request::get(format!("{origin}/slow?ms={SLOW_MS}")).body(Empty::<Bytes>::new());

    // Service binding: observe the handler.
    let before = SLOW_COMPLETED.load(Ordering::Relaxed);
    let binding = FetchService::from(env.service("SELF")?);
    let dropped = poll_briefly_then_drop(binding.clone().oneshot(slow_req()?)).await?;
    ensure(dropped, "slow fetch was still in flight at the drop")?;
    Delay::from(Duration::from_millis(SLOW_MS * 2)).await;
    ensure(
        SLOW_COMPLETED.load(Ordering::Relaxed) == before,
        "dropped fetch's handler did not complete",
    )?;

    // Control: an awaited fetch completes.
    let req = http::Request::get(format!("{origin}/slow?ms=10")).body(Empty::<Bytes>::new())?;
    let resp = binding.oneshot(req).await?;
    ensure(resp.status() == http::StatusCode::OK, "control status 200")?;
    resp.into_body().collect().await?;
    ensure(
        SLOW_COMPLETED.load(Ordering::Relaxed) == before + 1,
        "awaited fetch's handler completed",
    )?;

    // Global fetch: observe the signal. Last, because the dev server's proxy
    // lets this dropped fetch's handler run on and bump the counter later.
    let dropped = poll_briefly_then_drop(FetchService::new().oneshot(slow_req()?)).await?;
    ensure(dropped, "slow fetch was still in flight at the drop")?;
    let signal = last_fetch_signal()?;
    ensure(signal.aborted(), "dropping the future aborted the fetch")?;

    // Control: an awaited, fully read fetch does not abort anything.
    let req = http::Request::get(format!("{origin}/slow?ms=10")).body(Empty::<Bytes>::new())?;
    let resp = FetchService::new().oneshot(req).await?;
    resp.into_body().collect().await?;
    ensure(
        !last_fetch_signal()?.aborted(),
        "awaited and fully read fetch did not abort",
    )?;

    Ok(serde_json::json!({ "ok": true }))
}

/// Poll `fut` for 50ms, then drop it. `Ok(true)` if it was still pending.
async fn poll_briefly_then_drop<F>(fut: F) -> std::result::Result<bool, BoxError>
where
    F: std::future::Future<Output = std::result::Result<http::Response<Body>, Error>>,
{
    let fut = Box::pin(fut);
    let delay = Box::pin(Delay::from(Duration::from_millis(50)));
    match select(fut, delay).await {
        Either::Left((result, _)) => {
            result?;
            Ok(false)
        }
        Either::Right((_, fut)) => {
            drop(fut);
            Ok(true)
        }
    }
}

/// Dropping a half-read body aborts the fetch, which cancels the response
/// stream. Reading to the end, or unwrapping with `into_inner`, does not.
async fn test_drop_body(origin: &str) -> TestResult {
    const N: u32 = 20; // 25ms apart: 500ms to stream it all
    observe_fetch();
    let stream_req =
        || http::Request::get(format!("{origin}/echo-stream?n={N}")).body(Empty::<Bytes>::new());

    // Half-read, dropped: aborts.
    let resp = FetchService::new().oneshot(stream_req()?).await?;
    ensure(resp.status() == http::StatusCode::OK, "status 200")?;
    let signal = last_fetch_signal()?;
    let mut body = resp.into_body();
    let first = body
        .frame()
        .await
        .ok_or("stream ended before first frame")??;
    ensure(first.is_data(), "first frame is data")?;
    ensure(
        !signal.aborted(),
        "not aborted while the body is being read",
    )?;
    drop(body);
    ensure(signal.aborted(), "dropping the body aborted the fetch")?;

    // Read to the end: no abort, no error.
    let resp = FetchService::new().oneshot(stream_req()?).await?;
    let signal = last_fetch_signal()?;
    let collected = resp.into_body().collect().await?.to_bytes();
    ensure(!collected.is_empty(), "collected the whole stream")?;
    ensure(!signal.aborted(), "reading to the end did not abort")?;

    // `into_inner`: disarmed, the raw body is on its own.
    let resp = FetchService::new().oneshot(stream_req()?).await?;
    let signal = last_fetch_signal()?;
    drop(resp.into_body().into_inner());
    ensure(!signal.aborted(), "into_inner disarmed cancel-on-drop")?;

    Ok(serde_json::json!({ "ok": true }))
}

async fn test_binding(origin: &str, env: &Env) -> TestResult {
    echo_header_round_trip(FetchService::from(env.service("SELF")?), origin, "binding").await
}

async fn test_tower(origin: &str) -> TestResult {
    // ServiceBuilder + map_request/map_response, then BoxCloneService: this
    // only compiles if the service and its future are Send.
    let svc = ServiceBuilder::new()
        .map_request(|mut req: http::Request<Full<Bytes>>| {
            req.headers_mut()
                .insert("x-smoke", http::HeaderValue::from_static("tower"));
            req
        })
        .map_response(|mut resp: http::Response<Body>| {
            resp.headers_mut()
                .insert("x-mapped", http::HeaderValue::from_static("yes"));
            resp
        })
        .service(FetchService::new());
    let mut boxed: BoxCloneService<http::Request<Full<Bytes>>, http::Response<Body>, Error> =
        BoxCloneService::new(svc);
    let _second_handle = boxed.clone();

    let req = http::Request::get(format!("{origin}/echo")).body(Full::new(Bytes::new()))?;
    let resp = boxed.ready().await?.call(req).await?;
    ensure(resp.status() == http::StatusCode::OK, "status 200")?;
    ensure(
        resp.headers().get("x-mapped").is_some(),
        "map_response applied",
    )?;
    // `read_json` returns BoxError: proves Body::Error and Error are Into<BoxError>.
    let echo: Echo = read_json(resp.into_body()).await?;
    ensure(
        echo.headers.get("x-smoke").map(String::as_str) == Some("tower"),
        "map_request applied",
    )?;
    Ok(serde_json::json!({ "ok": true }))
}

// ---------------------------------------------------------------------------
// Shared test shapes

/// GET `/echo` through `svc` with `x-smoke: <marker>` and check the marker
/// comes back: the same round trip for global fetch and for a service binding.
async fn echo_header_round_trip(svc: FetchService, origin: &str, marker: &str) -> TestResult {
    let req = http::Request::get(format!("{origin}/echo"))
        .header("x-smoke", marker)
        .body(Empty::<Bytes>::new())?;

    let resp = svc.oneshot(req).await?;
    ensure(resp.status() == http::StatusCode::OK, "status 200")?;
    let echo: Echo = read_json(resp.into_body()).await?;
    ensure(echo.method == "GET", "method is GET")?;
    ensure(
        echo.headers.get("x-smoke").map(String::as_str) == Some(marker),
        "x-smoke header round-trips",
    )?;
    Ok(serde_json::json!({ "ok": true }))
}

#[derive(Clone, Copy)]
enum Cancellation {
    Abort,
    Timeout,
}

/// Fetch a slow route with `signal` in the extensions and check the failure
/// is reported as the expected kind of cancellation.
async fn expect_cancelled(origin: &str, signal: AbortSignal, kind: Cancellation) -> TestResult {
    let mut req =
        http::Request::get(format!("{origin}/slow?ms=5000")).body(Empty::<Bytes>::new())?;
    req.extensions_mut().insert(signal);

    let err = expect_err(FetchService::new().oneshot(req).await)?;
    ensure_on(err.stage() == Stage::Fetch, "stage is Fetch", &err)?;
    ensure_on(err.is_abort(), "is_abort()", &err)?;
    let expect_timeout = matches!(kind, Cancellation::Timeout);
    ensure_on(
        err.is_timeout() == expect_timeout,
        if expect_timeout {
            "is_timeout()"
        } else {
            "!is_timeout()"
        },
        &err,
    )?;
    Ok(serde_json::json!({
        "ok": true,
        "name": err.name(),
        "message": err.message(),
        "display": err.to_string(),
    }))
}

// ---------------------------------------------------------------------------
// Plumbing

/// `scheme://authority` of the incoming request; the Worker fetches itself.
fn origin_of(req: &HttpRequest) -> Result<String> {
    let uri = req.uri();
    match (uri.scheme_str(), uri.authority()) {
        (Some(scheme), Some(authority)) => Ok(format!("{scheme}://{authority}")),
        _ => Err(worker::Error::RustError(format!(
            "incoming request URI has no origin: {uri}"
        ))),
    }
}

fn query_param<T: std::str::FromStr>(req: &HttpRequest, key: &str) -> Option<T> {
    req.uri()
        .query()?
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == key)
        .and_then(|(_, v)| v.parse().ok())
}

async fn read_json<T: for<'de> Deserialize<'de>>(body: Body) -> std::result::Result<T, BoxError> {
    let bytes = body.collect().await?.to_bytes();
    Ok(serde_json::from_slice(&bytes)?)
}

fn ensure(cond: bool, what: &str) -> std::result::Result<(), BoxError> {
    if cond {
        Ok(())
    } else {
        Err(format!("assertion failed: {what}").into())
    }
}

/// `ensure` with the error under test in the failure message.
fn ensure_on(cond: bool, what: &str, err: &Error) -> std::result::Result<(), BoxError> {
    if cond {
        Ok(())
    } else {
        Err(format!(
            "assertion failed: {what} (error: {err}; name={:?}, code={:?})",
            err.name(),
            err.code()
        )
        .into())
    }
}

fn expect_err<T>(result: std::result::Result<T, Error>) -> std::result::Result<Error, BoxError> {
    match result {
        Ok(_) => Err("expected the fetch to fail, it succeeded".into()),
        Err(e) => Ok(e),
    }
}

fn report(result: TestResult) -> Result<SmokeResponse> {
    match result {
        Ok(value) => json(http::StatusCode::OK, &value),
        Err(err) => json(
            http::StatusCode::INTERNAL_SERVER_ERROR,
            &serde_json::json!({ "ok": false, "error": err.to_string() }),
        ),
    }
}

fn json<T: Serialize>(status: http::StatusCode, value: &T) -> Result<SmokeResponse> {
    let bytes = serde_json::to_vec(value)?;
    Ok(http::Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(full(Bytes::from(bytes)))?)
}

fn text(status: http::StatusCode, body: &'static str) -> Result<SmokeResponse> {
    Ok(http::Response::builder()
        .status(status)
        .header("content-type", "text/plain")
        .body(full(Bytes::from_static(body.as_bytes())))?)
}

fn full(bytes: Bytes) -> SmokeBody {
    Full::new(bytes)
        .map_err(|never: std::convert::Infallible| match never {})
        .boxed_unsync()
}
