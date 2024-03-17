use std::{
    fmt,
    future::Future,
    io, ops,
    pin::Pin,
    task::{Context, Poll},
};

#[cfg(unix)]
use tokio::io::{unix::AsyncFd, Interest};

use crate::{Acquired, TryAcquireClient};

#[cfg(unix)]
type AsyncAcquireClientInner = AsyncFd<TryAcquireClient>;

#[cfg(not(unix))]
type AsyncAcquireClientInner = TryAcquireClient;

/// Extension of [`Client`] that supports async acquire.
#[derive(Debug)]
pub struct AsyncAcquireClient(AsyncAcquireClientInner);

impl ops::Deref for AsyncAcquireClient {
    type Target = TryAcquireClient;

    fn deref(&self) -> &Self::Target {
        #[cfg(unix)]
        return self.0.get_ref();

        #[cfg(not(unix))]
        return &self.0;
    }
}

impl AsyncAcquireClient {
    /// Create async acquire client
    pub fn new(try_acquire_client: TryAcquireClient) -> io::Result<Self> {
        #[cfg(unix)]
        return AsyncFd::with_interest(try_acquire_client, Interest::READABLE).map(Self);

        #[cfg(not(unix))]
        return Ok(Self(try_acquire_client));
    }

    /// Deregisters and returns [`TryAcquireClient`]
    pub fn into_inner(self) -> TryAcquireClient {
        #[cfg(unix)]
        return self.0.into_inner();

        #[cfg(not(unix))]
        return self.0;
    }

    /// Async poll version of [`crate::Client::acquire`]
    pub fn poll_acquire(&self, cx: &mut Context<'_>) -> Poll<io::Result<Acquired>> {
        #[cfg(unix)]
        return loop {
            let mut ready_guard = match self.0.poll_read_ready(cx) {
                Poll::Pending => break Poll::Pending,
                Poll::Ready(res) => res?,
            };

            if let Some(acquired) = self.try_acquire()? {
                break Poll::Ready(Ok(acquired));
            } else {
                ready_guard.clear_ready();
            }
        };

        #[cfg(not(unix))]
        return self
            .inner
            .poll_acquire(cx)
            .map_ok(|data| Acquired::new(&self.0, data));
    }

    /// Async version of [`crate::Client::acquire`]
    pub fn acquire(&self) -> impl Future<Output = io::Result<Acquired>> + Send + Sync + Unpin + '_ {
        poll_fn(move |cx| self.poll_acquire(cx))
    }

    /// Async owned version of [`crate::Client::acquire`]
    pub fn acquire_owned(
        self,
    ) -> impl Future<Output = io::Result<Acquired>> + Send + Sync + Unpin + 'static {
        poll_fn(move |cx| self.poll_acquire(cx))
    }
}

// Code below is copied from https://doc.rust-lang.org/nightly/src/core/future/poll_fn.rs.html#143-153

fn poll_fn<T, F>(f: F) -> PollFn<F>
where
    F: FnMut(&mut Context<'_>) -> Poll<T>,
{
    PollFn { f }
}

#[must_use = "futures do nothing unless you `.await` or poll them"]
struct PollFn<F> {
    f: F,
}

impl<F: Unpin> Unpin for PollFn<F> {}

impl<F> fmt::Debug for PollFn<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PollFn").finish()
    }
}

impl<T, F> Future for PollFn<F>
where
    F: FnMut(&mut Context<'_>) -> Poll<T>,
{
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        // SAFETY: We are not moving out of the pinned field.
        (unsafe { &mut self.get_unchecked_mut().f })(cx)
    }
}
