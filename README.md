# tower-worker-fetch

[![CI](https://img.shields.io/github/actions/workflow/status/sagikazarmark/tower-worker-fetch/ci.yaml?style=flat-square)](https://github.com/sagikazarmark/tower-worker-fetch/actions/workflows/ci.yaml)
[![OpenSSF Scorecard](https://api.securityscorecards.dev/projects/github.com/sagikazarmark/tower-worker-fetch/badge?style=flat-square)](https://securityscorecards.dev/viewer/?uri=github.com/sagikazarmark/tower-worker-fetch)
[![crates.io](https://img.shields.io/crates/v/tower-worker-fetch?style=flat-square)](https://crates.io/crates/tower-worker-fetch)
[![docs.rs](https://img.shields.io/docsrs/tower-worker-fetch?style=flat-square)](https://docs.rs/tower-worker-fetch)

A [`tower::Service`](https://docs.rs/tower-service/latest/tower_service/trait.Service.html) over the Cloudflare Workers `fetch` API, typed with the [`http`](https://docs.rs/http) crate.

The Workers mirror of [`tower-reqwest`](https://crates.io/crates/tower-reqwest): a small, generic service whose `call` performs a
Workers `fetch`, so anything built around `http::Request` / `http::Response` and tower
(API clients, tonic-style stacks, your own middleware) runs inside a Worker without
knowing about `worker::Request`.

```rust,no_run
use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use tower::ServiceExt as _;
use tower_worker_fetch::FetchService;

# async fn example(env: worker::Env) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
let svc = FetchService::new();                       // global fetch
let svc = FetchService::from(env.service("API")?);   // or a service binding

let req = http::Request::post("https://example.com/things")
    .header("content-type", "application/json")
    .body(Full::new(Bytes::from_static(b"{}")))?;

let resp: http::Response<tower_worker_fetch::Body> = svc.oneshot(req).await?;
let status = resp.status();
let body = resp.into_body().collect().await?.to_bytes();
# let _ = (status, body);
# Ok(())
# }
```

## What you get

| | `tower-reqwest` | `tower-worker-fetch` |
|---|---|---|
| Request | `http::Request<B>` | `http::Request<B>`, `B: http_body::Body<Data = Bytes> + 'static` |
| Response | `http::Response<reqwest::Body>` | `http::Response<tower_worker_fetch::Body>` (streaming) |
| Error | `reqwest::Error` | `tower_worker_fetch::Error` (stage + JS error name/code/message, `Into<BoxError>`) |
| Transport | reqwest / hyper | Workers `fetch`, or a `worker::Fetcher` service binding |
| Cancellation | tower layers / tokio, cancel on drop | `Timeout` / `AbortSignal` in request extensions, cancel on drop |
| Target | native | `wasm32-unknown-unknown` on Cloudflare Workers |

The crate is deliberately glue: request and response conversion come from
`worker`'s `http` feature; this crate adds the `Service` impl, an error type that
records where the fetch failed, a body wrapper carrying that error type, and
documentation of what does and does not work on Workers.

## Composing with tower layers on Workers

The service, its future and the response body are `Send + Sync`, so the usual
tower plumbing applies:

```rust,no_run
use tower::{util::BoxCloneService, ServiceBuilder, ServiceExt as _};
use tower_worker_fetch::{Body, Error, FetchService};

# fn example() {
let svc = ServiceBuilder::new()
    .map_request(|mut req: http::Request<http_body_util::Full<bytes::Bytes>>| {
        req.headers_mut().insert("user-agent", "my-worker/1.0".parse().unwrap());
        req
    })
    .service(FetchService::new());

let boxed: BoxCloneService<_, http::Response<Body>, Error> = BoxCloneService::new(svc);
# let _ = boxed;
# }
```

Works: `map_request`, `map_response`, `map_err`, `then`, `retry` with a `Clone`
request body (`http_body_util::Full<Bytes>`), `BoxService`, `BoxCloneService`,
`BoxCloneSyncService`, and anything else that only needs `Send`.

Does **not** work: `tower::timeout`, `tower::buffer`, `tower::limit::rate`, and any
layer that uses `tokio::time` or spawns tasks. Those compile for wasm32 but panic at
first poll because there is no tokio runtime inside a Worker.

### Timeouts

Use the platform's timer instead of `tower::timeout`. Put a `Timeout` in the request
extensions:

```rust,no_run
use std::time::Duration;
use tower_worker_fetch::Timeout;

# fn example(req: &mut http::Request<()>) {
req.extensions_mut().insert(Timeout(Duration::from_secs(10)));
# }
```

The service turns it into an `AbortSignal.timeout()` on the outgoing request (whole
milliseconds, rounded up, saturated to `u32::MAX`; the runtime's `setTimeout` caps at
about 24.8 days regardless). A caller-built `AbortSignal` with a timeout in the
extensions still works, and the two combine: either one cancels the fetch.

The timeout spans the whole exchange, including reading the body; that is how
`AbortSignal.timeout()` works. A timeout that fires before the headers arrive fails
the response future with an `Error` whose `is_abort()` and `is_timeout()` are both
`true` (the runtime aborts with a `TimeoutError`, as the spec says) and `stage() ==
Stage::Fetch`. One that fires while the body is still streaming surfaces from the
`Body` with `stage() == Stage::Body` and the same `is_timeout()`. The alternative is
racing the response future against `worker::Delay` with
`futures_util::future::select`.

### Cancel on drop

As with reqwest, dropping the response future before it resolves aborts the fetch,
and dropping the response `Body` before end of stream cancels the underlying
`ReadableStream`. Nothing is required of you: every fetch gets its own
`AbortController`, combined with your `AbortSignal` extension (if any) via
`AbortSignal.any`. A body read to the end, or an empty one, has nothing left to
cancel. `Body::into_inner` disarms this: the returned `worker::Body` is on its own.

This abort is never observable as an `Error`: by the time it fires nothing is polling,
so `is_abort()` keeps meaning "the caller's signal or `Timeout` fired".

## Request extensions

The service honours two request extensions in 0.1:

| Extension | Effect |
|---|---|
| `Timeout(Duration)` | The fetch is aborted with a `TimeoutError` once the duration has elapsed, body included |
| [`AbortSignal`](https://docs.rs/worker/latest/worker/struct.AbortSignal.html) | Set as the request's `signal`; aborting it cancels the fetch |

Both may be present; they are combined with `AbortSignal.any`.

`worker::RequestRedirect` in extensions is **ignored**: `worker` has two
`RequestRedirect` types and its request conversion reads the private one, so there is
no way for a caller to set it. `worker::Cf` is passed through unchanged but is the
metadata of an *incoming* request, not an outbound option; this crate makes no promise
about it. Redirect control and outbound `cf` options (`cacheTtl`, `resolveOverride`,
...) are candidates for a later release.

## Bodies

**Request.** Any `B: http_body::Body<Data = Bytes> + 'static`. `http_body_util::Full<Bytes>`
and `Empty<Bytes>` are the usual choices. Every body that is not already at end of
stream is sent as a `ReadableStream`, including `Full<Bytes>`. That means the
subrequest carries no `Content-Length` unless you set the header yourself; if an
upstream rejects chunked requests, set `content-length` explicitly.

**Response.** `tower_worker_fetch::Body` implements `http_body::Body<Data = Bytes, Error = Error>`
and streams. Collect it with `http_body_util::BodyExt::collect`, or read it frame by
frame with `BodyExt::frame`. `Body::into_inner` gives you the underlying `worker::Body`
and disarms cancel-on-drop for it (see [Cancel on drop](#cancel-on-drop)).

## Errors

`Error` records the `Stage` at which the fetch failed (`Request`, `Fetch`, `Response`,
`Body`), the JavaScript error's `name` and `code` when present, the message, and the
`cause` chain (via `std::error::Error::source`). `Error::is_abort()` is `true` for any
fetch cancelled through an `AbortSignal`; `Error::is_timeout()` narrows that to
`AbortSignal.timeout()` (a `TimeoutError`). The runtime does not classify network
errors further: a connect or read timeout is a plain fetch failure, not `is_timeout()`.

## Threading

`FetchService`, `ResponseFuture` and `Body` are `Send + Sync`. What that rests on:

- `FetchService` holds a JavaScript handle (`JsValue`). wasm-bindgen marks `JsValue`
  `Send + Sync` on targets without the `atomics` feature, which is every Workers build.
- `ResponseFuture` wraps the in-flight future in `worker::send::SendFuture`. The future
  holds a `JsFuture` (`Rc`-based, `!Send`), the per-fetch `AbortController` handle
  behind cancel-on-drop (a `JsValue`, as above) and, until the request has been
  converted, your request body `B`, which has no `Send` bound.
- `Body` is `worker::Body`, which `worker` marks `Send + Sync` itself, plus the same
  `AbortController` handle once the response has arrived.

The reasoning is the one the `worker` crate uses throughout: Workers is single-threaded,
so these values never leave the isolate's thread. This crate contains no `unsafe` code
of its own. Because the `Send` claims only hold without `atomics`, building for a wasm
target with the `atomics` target feature enabled fails with a `compile_error!` that states
this assumption. If you need the type system to say `!Send`, open an issue; a `!Send` twin
would be additive.

## Host builds

The crate compiles on every target and works only on Cloudflare Workers. On a native
host every call panics inside wasm-bindgen. Compiling on the host exists so that
`cargo test`, `cargo doc`, rust-analyzer and downstream `cargo check` work without a
`--target`. If your crate has native alternatives, depend on it only for wasm32:

```toml
[target.'cfg(target_arch = "wasm32")'.dependencies]
tower-worker-fetch = "0.1"
```

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
