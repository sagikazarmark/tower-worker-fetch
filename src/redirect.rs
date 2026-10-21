//! The [`Redirect`] request extension.

/// Request extension: what the runtime does when the upstream answers with a
/// redirect.
///
/// Insert it into `request.extensions_mut()`. The service sets it as the
/// outgoing request's `redirect` mode and removes it from the extensions.
/// Without it, or with [`Redirect::Follow`], the runtime follows redirects
/// transparently and the caller never sees the 3xx.
///
/// ```rust,no_run
/// use tower_worker_fetch::Redirect;
///
/// # fn example(req: &mut http::Request<()>) {
/// req.extensions_mut().insert(Redirect::Manual);
/// # }
/// ```
///
/// There is no `error` mode: workerd rejects `redirect: "error"` outright
/// ("does not make sense at the edge; use `manual` and check the response
/// status code"). To fail on redirects, use [`Redirect::Manual`] and check
/// `response.status().is_redirection()`.
///
/// `worker::RequestRedirect` in the extensions is **not** honoured; use this
/// type.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Redirect {
    /// Follow redirects; the response is the one at the end of the chain.
    /// The runtime default.
    #[default]
    Follow,
    /// Return the 3xx response as-is, `Location` header included. Unlike a
    /// browser, workerd returns the real response, not an opaque one.
    Manual,
}

impl From<Redirect> for web_sys::RequestRedirect {
    fn from(redirect: Redirect) -> Self {
        match redirect {
            Redirect::Follow => web_sys::RequestRedirect::Follow,
            Redirect::Manual => web_sys::RequestRedirect::Manual,
        }
    }
}

/// Remove the [`Redirect`] extension, if any. `Some` only when the outgoing
/// request needs a mode other than the runtime default, so callers can leave
/// the default path untouched.
pub(crate) fn take_non_default(extensions: &mut http::Extensions) -> Option<Redirect> {
    match extensions.remove::<Redirect>() {
        Some(Redirect::Follow) | None => None,
        Some(redirect) => Some(redirect),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_is_send_sync_copy_default_static() {
        fn assert<T: Send + Sync + Copy + Default + 'static>() {}
        assert::<Redirect>();
    }

    #[test]
    fn default_is_follow() {
        assert_eq!(Redirect::default(), Redirect::Follow);
    }

    #[test]
    fn take_non_default_removes_the_extension() {
        let mut extensions = http::Extensions::new();
        assert_eq!(take_non_default(&mut extensions), None);

        extensions.insert(Redirect::Follow);
        assert_eq!(take_non_default(&mut extensions), None);
        assert!(extensions.get::<Redirect>().is_none());

        extensions.insert(Redirect::Manual);
        assert_eq!(take_non_default(&mut extensions), Some(Redirect::Manual));
        assert!(extensions.get::<Redirect>().is_none());
    }
}
