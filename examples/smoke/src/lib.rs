//! Smoke test Worker for `tower-worker-fetch`.
//!
//! Runs under `wrangler dev` and fetches its own origin, so it only makes sense
//! locally or in CI. Every `/test-*` route returns `200 {"ok":true,...}` on
//! success and `500 {"ok":false,"error":...}` on failure. `run.sh` drives it.

use std::collections::BTreeMap;
use std::time::Duration;

use base64::Engine as _;
use bytes::Bytes;
use futures_util::StreamExt as _;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Empty, Full};
use serde::{Deserialize, Serialize};
use tower::util::BoxCloneService;
use tower::{Service as _, ServiceBuilder, ServiceExt as _};
use tower_worker_fetch::{AbortSignal, Body, Error, FetchService, Stage};
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
    text(http::StatusCode::OK, "slow")
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
