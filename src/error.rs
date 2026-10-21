//! Error type for [`FetchService`](crate::FetchService) and [`Body`](crate::Body).

use std::fmt;

use worker::js_sys::Reflect;
use worker::wasm_bindgen::JsValue;

/// Where in a fetch an [`Error`] occurred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Stage {
    /// Building the outgoing `web_sys::Request` from the `http::Request`.
    Request,
    /// The `fetch` call itself: network errors, aborts, a service binding
    /// throwing, the returned value not being a `Response`.
    Fetch,
    /// Converting the `web_sys::Response` into an `http::Response`.
    Response,
    /// Reading the response body stream.
    Body,
}

impl Stage {
    fn as_str(self) -> &'static str {
        match self {
            Stage::Request => "request",
            Stage::Fetch => "fetch",
            Stage::Response => "response",
            Stage::Body => "body",
        }
    }
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An error from a fetch performed by [`FetchService`](crate::FetchService),
/// or from reading a [`Body`](crate::Body).
///
/// A small, string-only error rather than `worker::Error`: it records the
/// [`Stage`] at which the fetch failed, which `worker::Error` cannot express,
/// it is `Clone`, and it does not change shape when `worker::Error` (a
/// `#[non_exhaustive]` enum) grows variants. It is `Send + Sync`
/// unconditionally, where `worker::Error` is `Send` only because wasm-bindgen
/// marks `JsValue` `Send` on targets without the `atomics` feature.
///
/// From a JavaScript error it keeps the `name` (for example `"AbortError"` or
/// `"TypeError"`), the `code` property when present, the message, and the
/// `cause` chain, as far as `worker::Error` preserves them. (`worker` folds a
/// few email-subsystem error codes into dedicated variants; those arrive here
/// with their `Display` text and no `name`/`code`.)
///
/// The Workers runtime does not classify network failures beyond that, so
/// there is no `is_dns()` or similar. [`Error::is_abort`] and
/// [`Error::is_timeout`] are reliable because `AbortSignal` cancellations
/// always carry `name == "AbortError"` or `"TimeoutError"`.
#[derive(Debug, Clone)]
pub struct Error {
    stage: Stage,
    name: Option<String>,
    code: Option<String>,
    message: String,
    cause: Option<Box<Error>>,
}

impl Error {
    /// The stage of the fetch at which the error occurred.
    pub fn stage(&self) -> Stage {
        self.stage
    }

    /// The JavaScript error's `name`, when the error came from JavaScript and
    /// had one. `"AbortError"`, `"TimeoutError"`, `"TypeError"` are typical.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// The JavaScript error's `code` property, when present.
    pub fn code(&self) -> Option<&str> {
        self.code.as_deref()
    }

    /// The error message, without stage or name prefix.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// `true` if the fetch was cancelled through an `AbortSignal`.
    ///
    /// Covers both ways a signal gets aborted: `AbortController::abort()`
    /// (`name == "AbortError"`) and `AbortSignal.timeout()`
    /// (`name == "TimeoutError"`; see [`Error::is_timeout`]). A signal aborted
    /// with a custom reason (`AbortController::abort_with_reason`) rejects the
    /// fetch with that reason instead, which this method cannot recognise.
    pub fn is_abort(&self) -> bool {
        matches!(self.name.as_deref(), Some("AbortError" | "TimeoutError"))
    }

    /// `true` if the fetch was cancelled by an `AbortSignal` created with
    /// `AbortSignal.timeout()`, or otherwise aborted with a `TimeoutError`.
    ///
    /// This is the only timeout the runtime reports by name. Network-level
    /// timeouts (connect, read) surface as generic fetch failures and are not
    /// distinguishable.
    pub fn is_timeout(&self) -> bool {
        self.name.as_deref() == Some("TimeoutError")
    }

    fn new(
        stage: Stage,
        name: Option<String>,
        code: Option<String>,
        message: String,
        cause: Option<Box<Error>>,
    ) -> Self {
        Error {
            stage,
            name,
            code,
            message,
            cause,
        }
    }

    pub(crate) fn from_message(stage: Stage, message: &str) -> Self {
        Error::new(stage, None, None, message.to_owned(), None)
    }

    pub(crate) fn from_worker(stage: Stage, err: worker::Error) -> Self {
        match err {
            worker::Error::UnknownJsError {
                name,
                message,
                code,
                cause,
                ..
            } => Error::new(
                stage,
                name,
                code,
                message,
                cause.map(|c| Box::new(Error::from_worker(stage, *c))),
            ),
            // A thrown value that is neither an `Error` instance nor a string.
            // Try the conventional properties anyway (a `DOMException` that
            // fails `instanceof Error` on some runtime would land here), then
            // fall back to the value's debug rendering.
            worker::Error::Internal(value) => {
                let name = js_string_property(&value, "name");
                let code = js_string_property(&value, "code");
                let message =
                    js_string_property(&value, "message").unwrap_or_else(|| format!("{value:?}"));
                Error::new(stage, name, code, message, None)
            }
            worker::Error::JsError(message) | worker::Error::RustError(message) => {
                Error::new(stage, None, None, message, None)
            }
            other => Error::new(stage, None, None, other.to_string(), None),
        }
    }

    pub(crate) fn from_js(stage: Stage, value: JsValue) -> Self {
        Error::from_worker(stage, worker::Error::from(value))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: ", self.stage)?;
        if let Some(name) = &self.name {
            write!(f, "{name}")?;
            if let Some(code) = &self.code {
                write!(f, " [{code}]")?;
            }
            write!(f, ": ")?;
        }
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.cause
            .as_deref()
            .map(|c| c as &(dyn std::error::Error + 'static))
    }
}

/// Read a string-valued property off an arbitrary JS value. `Reflect.get` on
/// a non-object throws, which comes back as `Err` and therefore `None`.
fn js_string_property(value: &JsValue, key: &str) -> Option<String> {
    Reflect::get(value, &JsValue::from_str(key))
        .ok()
        .and_then(|v| v.as_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_name(stage: Stage, name: &str, code: Option<&str>, message: &str) -> Error {
        Error::new(
            stage,
            Some(name.to_owned()),
            code.map(String::from),
            message.to_owned(),
            None,
        )
    }

    #[test]
    fn error_is_send_sync_static() {
        fn assert<T: Send + Sync + 'static>() {}
        assert::<Error>();
    }

    #[test]
    fn converts_into_box_error() {
        let err = with_name(Stage::Fetch, "TypeError", None, "failed");
        let boxed: Box<dyn std::error::Error + Send + Sync> = err.into();
        assert_eq!(boxed.to_string(), "fetch: TypeError: failed");
    }

    #[test]
    fn display_without_name() {
        let err = Error::from_message(Stage::Request, "bad uri");
        assert_eq!(err.to_string(), "request: bad uri");
    }

    #[test]
    fn display_with_name_and_code() {
        let err = with_name(Stage::Fetch, "Error", Some("E_SOMETHING"), "boom");
        assert_eq!(err.to_string(), "fetch: Error [E_SOMETHING]: boom");
    }

    #[test]
    fn is_abort_for_abort_signal_names() {
        assert!(with_name(Stage::Fetch, "AbortError", None, "aborted").is_abort());
        assert!(with_name(Stage::Fetch, "TimeoutError", None, "timed out").is_abort());
        assert!(!with_name(Stage::Fetch, "TypeError", None, "aborted").is_abort());
        assert!(!Error::from_message(Stage::Fetch, "AbortError").is_abort());
    }

    #[test]
    fn is_timeout_only_for_timeout_error_name() {
        assert!(with_name(Stage::Fetch, "TimeoutError", None, "timed out").is_timeout());
        assert!(!with_name(Stage::Fetch, "AbortError", None, "aborted").is_timeout());
        assert!(!Error::from_message(Stage::Fetch, "TimeoutError").is_timeout());
    }

    #[test]
    fn accessors() {
        let err = with_name(Stage::Body, "TypeError", Some("X"), "msg");
        assert_eq!(err.stage(), Stage::Body);
        assert_eq!(err.name(), Some("TypeError"));
        assert_eq!(err.code(), Some("X"));
        assert_eq!(err.message(), "msg");
    }

    #[test]
    fn source_walks_cause_chain() {
        let leaf = with_name(Stage::Fetch, "Error", None, "leaf");
        let root = Error::new(
            Stage::Fetch,
            Some("Error".to_owned()),
            None,
            "root".to_owned(),
            Some(Box::new(leaf)),
        );

        let source = std::error::Error::source(&root).expect("has a cause");
        assert_eq!(source.to_string(), "fetch: Error: leaf");
        assert!(std::error::Error::source(source).is_none());
    }

    #[test]
    fn from_worker_rust_error() {
        let err = Error::from_worker(Stage::Request, worker::Error::RustError("nope".into()));
        assert_eq!(err.stage(), Stage::Request);
        assert_eq!(err.name(), None);
        assert_eq!(err.message(), "nope");
        assert_eq!(err.to_string(), "request: nope");
    }

    #[test]
    fn from_worker_js_error_string() {
        let err = Error::from_worker(Stage::Fetch, worker::Error::JsError("thrown".into()));
        assert_eq!(err.name(), None);
        assert_eq!(err.message(), "thrown");
    }

    #[test]
    fn from_worker_http_error_uses_display() {
        let http_err = http::Request::builder()
            .uri("not a uri")
            .body(())
            .expect_err("invalid uri");
        let err = Error::from_worker(Stage::Request, worker::Error::Http(http_err));
        assert_eq!(err.stage(), Stage::Request);
        assert!(!err.message().is_empty());
    }

    #[test]
    fn from_worker_other_variant_uses_display() {
        let err = Error::from_worker(Stage::Response, worker::Error::BodyUsed);
        assert_eq!(err.message(), worker::Error::BodyUsed.to_string());
    }

    #[test]
    fn stage_display() {
        assert_eq!(Stage::Request.to_string(), "request");
        assert_eq!(Stage::Fetch.to_string(), "fetch");
        assert_eq!(Stage::Response.to_string(), "response");
        assert_eq!(Stage::Body.to_string(), "body");
    }
}
