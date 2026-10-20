//! Streaming response body.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Frame, SizeHint};
use pin_project_lite::pin_project;

use crate::error::{Error, Stage};

pin_project! {
    /// The streaming body of a response returned by
    /// [`FetchService`](crate::FetchService).
    ///
    /// A thin wrapper over [`worker::Body`] whose job is the error type:
    /// `worker::Body` yields `worker::Error`; this body yields [`Error`], so
    /// the stage (`Stage::Body`) is recorded and `BodyExt::collect().await?`
    /// works in functions returning `Box<dyn Error + Send + Sync>` or
    /// `anyhow::Result` without a `map_err` at every call site. It also keeps
    /// `worker::Body` out of this crate's public API.
    ///
    /// Collect it with [`http_body_util::BodyExt::collect`] or consume it
    /// frame by frame with [`http_body_util::BodyExt::frame`].
    ///
    /// [`http_body_util::BodyExt::collect`]: https://docs.rs/http-body-util/latest/http_body_util/trait.BodyExt.html#method.collect
    /// [`http_body_util::BodyExt::frame`]: https://docs.rs/http-body-util/latest/http_body_util/trait.BodyExt.html#method.frame
    #[derive(Debug)]
    pub struct Body {
        #[pin]
        inner: worker::Body,
    }
}

impl Body {
    pub(crate) fn new(inner: worker::Body) -> Self {
        Self { inner }
    }

    /// Unwrap into the underlying [`worker::Body`], for callers who want the
    /// raw `ReadableStream` (`worker::Body::into_inner`) or a `worker::Error`.
    pub fn into_inner(self) -> worker::Body {
        self.inner
    }
}

impl http_body::Body for Body {
    type Data = Bytes;
    type Error = Error;

    #[inline]
    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        self.project()
            .inner
            .poll_frame(cx)
            .map_err(|e| Error::from_worker(Stage::Body, e))
    }

    #[inline]
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    #[inline]
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body::Body as _;

    #[test]
    fn body_is_send_sync_static() {
        fn assert<T: Send + Sync + 'static>() {}
        assert::<Body>();
    }

    #[test]
    fn empty_body_delegates() {
        let body = Body::new(worker::Body::empty());
        assert!(body.is_end_stream());
        assert_eq!(body.size_hint().exact(), Some(0));

        let mut body = std::pin::pin!(body);
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        assert!(matches!(
            body.as_mut().poll_frame(&mut cx),
            Poll::Ready(None)
        ));
    }

    #[test]
    fn into_inner_round_trips() {
        let body = Body::new(worker::Body::empty());
        let inner = body.into_inner();
        assert!(inner.is_end_stream());
    }
}
