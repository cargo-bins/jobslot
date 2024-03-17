use std::{
    borrow::Cow,
    io,
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError},
    task::{Context, Poll, Waker},
};

#[derive(Debug)]
pub struct Client {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    count: Mutex<usize>,
    cvar: Condvar,
    wakers: Mutex<Vec<Waker>>,
}

#[derive(Debug)]
pub struct Acquired(());

impl Client {
    pub fn new(limit: usize) -> io::Result<Client> {
        Ok(Client {
            inner: Arc::new(Inner {
                count: Mutex::new(limit),
                cvar: Condvar::new(),
                wakers: Mutex::default(),
            }),
        })
    }

    pub unsafe fn open(_s: &[u8]) -> Option<Client> {
        None
    }

    fn count(&self) -> MutexGuard<'_, usize> {
        self.inner
            .count
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub fn acquire(&self) -> io::Result<Acquired> {
        let mut lock = self.count();
        while *lock == 0 {
            lock = self
                .inner
                .cvar
                .wait(lock)
                .unwrap_or_else(PoisonError::into_inner);
        }
        *lock -= 1;
        Ok(Acquired(()))
    }

    pub fn try_acquire(&self) -> io::Result<Option<Acquired>> {
        let mut lock = self.count();
        if *lock == 0 {
            Ok(None)
        } else {
            *lock -= 1;
            Ok(Some(Acquired(())))
        }
    }

    fn wakers(&self) -> MutexGuard<'_, Vec<Waker>> {
        self.inner
            .wakers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub fn poll_acquire(&self, cx: &mut Context<'_>) -> Poll<io::Result<Acquired>> {
        let mut lock = self.count();

        if *lock == 0 {
            // Obtain wakers within critical section of count,
            // to make sure no one else can release any token
            // until our waker is added, otherwise it is possible
            // for release to be called without waking us up.
            //
            // Afterwards, anyone who release the token will
            // wake us up.
            self.wakers().push(cx.waker().clone());
            Poll::Pending
        } else {
            *lock -= 1;
            Poll::Ready(Ok(Some(Acquired(()))))
        }
    }

    pub fn release(&self, _data: Option<&Acquired>) -> io::Result<()> {
        let mut lock = self.count();
        *lock += 1;
        drop(lock);

        // Wake up, even if the lock might not be enough for everyone,
        // it still has to wake up all async wakers to prevent any of
        // them from beinmg asleep forever.
        //
        // It's ok to not hold the lock of count, the worst case scenario
        // is they will add themselves back to the queue again.
        self.inner.cvar.notify_one();
        self.wakers().drain(..).for_each(Waker::wake);

        Ok(())
    }

    pub fn string_arg(&self) -> Cow<'_, str> {
        panic!(
            "On this platform there is no cross process jobserver support,
             so Client::configure_and_run is not supported."
        );
    }

    pub fn pre_run<Cmd>(&self, _cmd: &mut Cmd) {
        panic!(
            "On this platform there is no cross process jobserver support,
             so Client::configure_and_run is not supported."
        );
    }

    pub fn available(&self) -> io::Result<usize> {
        Ok(*self.count())
    }
}
