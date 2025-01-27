//! An implementation of the GNU make jobserver.
//!
//! This crate is an implementation, in Rust, of the GNU `make` jobserver for
//! CLI tools that are interoperating with make or otherwise require some form
//! of parallelism limiting across process boundaries. This was originally
//! written for usage in Cargo to both (a) work when `cargo` is invoked from
//! `make` (using `make`'s jobserver) and (b) work when `cargo` invokes build
//! scripts, exporting a jobserver implementation for `make` processes to
//! transitively use.
//!
//! The jobserver implementation can be found in [detail online][docs] but
//! basically boils down to a cross-process semaphore. On Unix this is
//! implemented with the `pipe` syscall and read/write ends of a pipe and on
//! Windows this is implemented literally with IPC semaphores.
//!
//! Starting from GNU `make` version 4.4, named pipe becomes the default way
//! in communication on Unix, which is supported by this crate.
//!
//! However, [`Client::configure_and_run`] and [`Client::configure_make_and_run`]
//! still use the old syntax to keep backwards compatibility with existing
//! programs, e.g. make < 4.4.
//!
//! To create a new fifo on unix, use [`Client::new_with_fifo`] and to use it
//! for spawning process, use [`Client::configure_and_run_with_fifo`] or
//! [`Client::configure_make_and_run_with_fifo`].
//!
//! The jobserver protocol in `make` also dictates when tokens are acquired to
//! run child work, and clients using this crate should take care to implement
//! such details to ensure correct interoperation with `make` itself.
//!
//! ## Advantages over `jobserver`?
//!
//!  - `jobslot` contains bug fix for [Client::configure is unsafe]
//!  - `jobslot` removed use of signal handling in the helper thread on unix
//!  - `jobslot` uses `winapi` on windows instead of manually declaring bindings (some of the bindings seem to be wrong)
//!  - `jobslot` uses `getrandom` on windows instead of making homebrew one using raw windows api
//!  - `jobslot::Client::from_env` can be called any number of times on Windows and Unix.
//!
//! [Client::configure is unsafe]: https://github.com/alexcrichton/jobserver-rs/issues/25
//!
//!
//! ## Examples
//!
//! Connect to a jobserver that was set up by `make` or a different process:
//!
//! ```no_run
//! use jobslot::Client;
//!
//! // See API documentation for why this is `unsafe`
//! let client = match unsafe { Client::from_env() } {
//!     Some(client) => client,
//!     None => panic!("client not configured"),
//! };
//! ```
//!
//! Acquire and release token from a jobserver:
//!
//! ```no_run
//! use jobslot::Client;
//!
//! let client = unsafe { Client::from_env().unwrap() };
//! let token = client.acquire().unwrap(); // blocks until it is available
//! drop(token); // releases the token when the work is done
//! ```
//!
//! Create a new jobserver and configure a child process to have access:
//!
//! ```
//! use std::process::Command;
//! use jobslot::Client;
//!
//! let client = Client::new(4).expect("failed to create jobserver");
//! let mut cmd = Command::new("make");
//! let child = client.configure_and_run(&mut cmd, |cmd| cmd.spawn()).unwrap();
//! ```
//!
//! ## Features
//!
//!  - tokio: This would enable support of `tokio::process::Command`.
//!    You would be able to write:
//!
//!    ```
//!    use tokio::process::Command;
//!    use jobslot::Client;
//!
//!    # #[tokio::main]
//!    # async fn main() {
//!    let client = Client::new(4).expect("failed to create jobserver");
//!    let mut cmd = Command::new("make");
//!    let child = client.configure_and_run(&mut cmd, |cmd| cmd.spawn()).unwrap();
//!    # }
//!    ```
//!
//! ## Caveats
//!
//! This crate makes no attempt to release tokens back to a jobserver on
//! abnormal exit of a process. If a process which acquires a token is killed
//! with ctrl-c or some similar signal then tokens will not be released and the
//! jobserver may be in a corrupt state.
//!
//! Note that this is typically ok as ctrl-c means that an entire build process
//! is being torn down, but it's worth being aware of at least!
//!
//! ## Windows caveats
//!
//! There appear to be two implementations of `make` on Windows. On MSYS2 one
//! typically comes as `mingw32-make` and the other as `make` itself. I'm not
//! personally too familiar with what's going on here, but for jobserver-related
//! information the `mingw32-make` implementation uses Windows semaphores
//! whereas the `make` program does not. The `make` program appears to use file
//! descriptors and I'm not really sure how it works, so this crate is not
//! compatible with `make` on Windows. It is, however, compatible with
//! `mingw32-make`.
//!
//! [docs]: http://make.mad-scientist.net/papers/jobserver-implementation/

#![deny(missing_docs, missing_debug_implementations)]
// only enables the nightly `doc_auto_cfg` feature when
// the `docsrs` configuration attribute is defined
#![cfg_attr(docsrs, feature(doc_auto_cfg))]

use std::{
    env,
    error::Error as StdError,
    ffi, fmt, io, ops, process,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use cfg_if::cfg_if;
use scopeguard::{guard, ScopeGuard};

cfg_if! {
    if #[cfg(unix)] {
        #[path = "unix.rs"]
        mod imp;
    } else if #[cfg(windows)] {
        #[path = "windows.rs"]
        mod imp;
    } else if #[cfg(not(any(unix, windows)))] {
        #[path = "wasm.rs"]
        mod imp;
    }
}

#[cfg(any(all(feature = "tokio", unix), not(any(unix, windows))))]
mod async_client;
#[cfg(any(all(feature = "tokio", unix), not(any(unix, windows))))]
pub use async_client::AsyncAcquireClient;

/// Command that can be accepted by this crate.
pub trait Command {
    /// Inserts or updates an environment variable mapping.
    fn env<K, V>(&mut self, key: K, val: V) -> &mut Self
    where
        K: AsRef<ffi::OsStr>,
        V: AsRef<ffi::OsStr>;

    /// Removes an environment variable mapping.
    fn env_remove<K: AsRef<ffi::OsStr>>(&mut self, key: K) -> &mut Self;

    /// Schedules a closure to be run just before the exec function is invoked.
    ///
    /// Check [`std::os::unix::process::CommandExt::pre_exec`]
    /// for more information.
    ///
    /// # Safety
    ///
    /// Same as [`std::os::unix::process::CommandExt::pre_exec`].
    #[cfg(unix)]
    unsafe fn pre_exec<F>(&mut self, f: F) -> &mut Self
    where
        F: FnMut() -> io::Result<()> + Send + Sync + 'static;
}
impl Command for process::Command {
    fn env<K, V>(&mut self, key: K, val: V) -> &mut Self
    where
        K: AsRef<ffi::OsStr>,
        V: AsRef<ffi::OsStr>,
    {
        process::Command::env(self, key.as_ref(), val.as_ref())
    }

    fn env_remove<K: AsRef<ffi::OsStr>>(&mut self, key: K) -> &mut Self {
        process::Command::env_remove(self, key.as_ref())
    }

    #[cfg(unix)]
    unsafe fn pre_exec<F>(&mut self, f: F) -> &mut Self
    where
        F: FnMut() -> io::Result<()> + Send + Sync + 'static,
    {
        use std::os::unix::process::CommandExt;
        CommandExt::pre_exec(self, f)
    }
}
#[cfg(feature = "tokio")]
impl Command for tokio::process::Command {
    fn env<K, V>(&mut self, key: K, val: V) -> &mut Self
    where
        K: AsRef<ffi::OsStr>,
        V: AsRef<ffi::OsStr>,
    {
        tokio::process::Command::env(self, key.as_ref(), val.as_ref())
    }

    fn env_remove<K: AsRef<ffi::OsStr>>(&mut self, key: K) -> &mut Self {
        tokio::process::Command::env_remove(self, key.as_ref())
    }

    #[cfg(unix)]
    unsafe fn pre_exec<F>(&mut self, f: F) -> &mut Self
    where
        F: FnMut() -> io::Result<()> + Send + Sync + 'static,
    {
        tokio::process::Command::pre_exec(self, f)
    }
}
impl<T: Command> Command for &mut T {
    fn env<K, V>(&mut self, key: K, val: V) -> &mut Self
    where
        K: AsRef<ffi::OsStr>,
        V: AsRef<ffi::OsStr>,
    {
        (*self).env(key.as_ref(), val.as_ref());
        self
    }

    fn env_remove<K: AsRef<ffi::OsStr>>(&mut self, key: K) -> &mut Self {
        (*self).env_remove(key.as_ref());
        self
    }

    #[cfg(unix)]
    unsafe fn pre_exec<F>(&mut self, f: F) -> &mut Self
    where
        F: FnMut() -> io::Result<()> + Send + Sync + 'static,
    {
        (*self).pre_exec(f);
        self
    }
}

/// Returns RAII to ensure env_remove is called on unwinding
fn setup_envs<'a, Cmd>(
    mut cmd: Cmd,
    envs: &'a [&'a str],
    value: &ffi::OsStr,
) -> ScopeGuard<Cmd, impl FnOnce(Cmd) + 'a>
where
    Cmd: Command,
{
    // Setup env
    for env in envs {
        cmd.env(env, value);
    }

    // Use RAII to ensure env_remove is called on unwinding
    guard(cmd, move |mut cmd| {
        for env in envs {
            cmd.env_remove(env);
        }
    })
}

#[derive(Debug)]
struct ClientInner {
    inner: imp::Client,
    #[cfg(unix)]
    acitve_try_acquire_client_count: Mutex<usize>,
}

impl ClientInner {
    #[cfg(unix)]
    fn acitve_try_acquire_client_count(&self) -> MutexGuard<'_, usize> {
        self.acitve_try_acquire_client_count
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// A client of a jobserver
///
/// This structure is the main type exposed by this library, and is where
/// interaction to a jobserver is configured through. Clients are either created
/// from scratch in which case the internal semphore is initialied on the spot,
/// or a client is created from the environment to connect to a jobserver
/// already created.
///
/// Some usage examples can be found in the crate documentation for using a
/// client.
///
/// Note that a `Client` implements the `Clone` trait, and all instances of a
/// `Client` refer to the same jobserver instance.
#[derive(Clone, Debug)]
pub struct Client(Arc<ClientInner>);

impl Client {
    /// Creates a new jobserver initialized with the given parallelism limit.
    ///
    /// A client to the jobserver created will be returned. This client will
    /// allow at most `limit` tokens to be acquired from it in parallel. More
    /// calls to `acquire` will cause the calling thread to block.
    ///
    /// Note that the created `Client` is not automatically inherited into
    /// spawned child processes from this program. Manual usage of the
    /// `configure` function is required for a child process to have access to a
    /// job server.
    ///
    /// # Examples
    ///
    /// ```
    /// use jobslot::Client;
    ///
    /// let client = Client::new(4).expect("failed to create jobserver");
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if any I/O error happens when attempting to create the
    /// jobserver client.
    pub fn new(limit: usize) -> io::Result<Self> {
        imp::Client::new(limit).map(Self::new_inner)
    }

    /// Same as [`Client::new`] except that it will create a named fifo on
    /// unix so that you can use [`Client::configure_and_run_with_fifo`] or
    /// [`Client::configure_make_and_run_with_fifo`] to pass the fifo
    /// instead of fds.
    pub fn new_with_fifo(limit: usize) -> io::Result<Self> {
        #[cfg(unix)]
        {
            imp::Client::new_fifo(limit).map(Self::new_inner)
        }
        #[cfg(not(unix))]
        {
            Self::new(limit)
        }
    }

    fn new_inner(inner: imp::Client) -> Self {
        Self(Arc::new(ClientInner {
            inner,
            #[cfg(unix)]
            acitve_try_acquire_client_count: Mutex::default(),
        }))
    }

    /// Attempts to connect to the jobserver specified in this process's
    /// environment.
    ///
    /// When the a `make` executable calls a child process it will configure the
    /// environment of the child to ensure that it has handles to the jobserver
    /// it's passing down. This function will attempt to look for these details
    /// and connect to the jobserver.
    ///
    /// Note that the created `Client` is not automatically inherited into
    /// spawned child processes from this program. Manual usage of the
    /// [`Client::configure_and_run`] or [`Client::configure_make_and_run`]
    /// function is required for a child process to have access to a job server.
    ///
    /// # Return value
    ///
    /// If a jobserver was found in the environment and it looks correct then
    /// `Some` of the connected client will be returned. If no jobserver was
    /// found then `None` will be returned.
    ///
    /// Note that on Unix  this function will configure the file descriptors
    /// with `CLOEXEC` so they're not automatically inherited by spawned
    /// children.
    ///
    /// Jobservers on Unix are implemented with `pipe` file descriptors,
    /// and they're inherited from parent processes.
    ///
    /// # Safety
    ///
    /// This function is `unsafe` to call on Unix specifically as it
    /// transitively requires usage of the `from_raw_fd` function, which is
    /// itself unsafe in some circumstances.
    ///
    /// It's recommended to call this function very early in the lifetime of a
    /// program before any other file descriptors are opened. That way you can
    /// make sure to take ownership properly of the file descriptors passed
    /// down, if any.
    ///
    /// Note, though, that on Windows and Unix it should be safe to
    /// call this function any number of times.
    pub unsafe fn from_env() -> Option<Self> {
        let var = env::var_os("CARGO_MAKEFLAGS")
            .or_else(|| env::var_os("MAKEFLAGS"))
            .or_else(|| env::var_os("MFLAGS"))?;

        let var = {
            cfg_if! {
                if #[cfg(unix)] {
                    std::os::unix::ffi::OsStrExt::as_bytes(var.as_os_str())
                } else {
                    var.to_str()?.as_bytes()
                }
            }
        };
        let makeflags = var.split(u8::is_ascii_whitespace);

        // `--jobserver-auth=` is the only documented makeflags.
        // `--jobserver-fds=` is actually an internal only makeflags, so we should
        // always prefer `--jobserver-auth=`.
        //
        // Also, according to doc of makeflags, if there are multiple `--jobserver-auth=`
        // the last one is used
        if let Some(flag) = makeflags
            .clone()
            .filter_map(|s| s.strip_prefix(b"--jobserver-auth="))
            .last()
        {
            imp::Client::open(flag)
        } else {
            imp::Client::open(
                makeflags
                    .filter_map(|s| s.strip_prefix(b"--jobserver-fds="))
                    .last()?,
            )
        }
        .map(Self::new_inner)
    }

    /// Acquires a token from this jobserver client.
    ///
    /// This function will block the calling thread until a new token can be
    /// acquired from the jobserver.
    ///
    /// # Return value
    ///
    /// On successful acquisition of a token an instance of `Acquired` is
    /// returned. This structure, when dropped, will release the token back to
    /// the jobserver. It's recommended to avoid leaking this value.
    ///
    /// # Errors
    ///
    /// If an I/O error happens while acquiring a token then this function will
    /// return immediately with the error. If an error is returned then a token
    /// was not acquired.
    pub fn acquire(&self) -> io::Result<Acquired> {
        let data = self.0.inner.acquire()?;
        Ok(Acquired::new(self, data))
    }

    /// Returns amount of tokens in the read-side pipe.
    ///
    /// # Return value
    ///
    /// Number of bytes available to be read from the jobserver pipe
    ///
    /// # Errors
    ///
    /// Underlying errors from the ioctl will be passed up.
    pub fn available(&self) -> io::Result<usize> {
        self.0.inner.available()
    }

    /// Configures a child process to have access to this client's jobserver as
    /// well and run the `f` which spawns the process.
    ///
    /// NOTE that you have to spawn the process inside `f`, otherwise the jobserver
    /// would not be inherited.
    ///
    /// This function is required to be called to ensure that a jobserver is
    /// properly inherited to a child process. If this function is *not* called
    /// then this `Client` will not be accessible in the child process. In other
    /// words, if not called, then `Client::from_env` will return `None` in the
    /// child process (or the equivalent of `Child::from_env` that `make` uses).
    ///
    /// ## Environment variables
    ///
    /// This function only sets up `CARGO_MAKEFLAGS`, which is used by
    /// `cargo`.
    ///
    /// ## Platform-specific behavior
    ///
    /// On Unix and Windows this will clobber the `CARGO_MAKEFLAGS` environment
    /// variables for the child process, and on Unix this will also allow the
    /// two file descriptors for this client to be inherited to the child.
    ///
    /// On platforms other than Unix and Windows this panics.
    pub fn configure_and_run<Cmd, F, R>(&self, cmd: Cmd, f: F) -> io::Result<R>
    where
        Cmd: Command,
        F: FnOnce(&mut Cmd) -> io::Result<R>,
    {
        self.configure_and_run_inner(cmd, f, &["CARGO_MAKEFLAGS"])
    }

    /// Same as [`Client::configure_and_run`] except that it sets up environment
    /// variables `CARGO_MAKEFLAGS`, `MAKEFLAGS` and `MFLAGS`, which is used by
    /// `cargo` and `make`.
    pub fn configure_make_and_run<Cmd, F, R>(&self, cmd: Cmd, f: F) -> io::Result<R>
    where
        Cmd: Command,
        F: FnOnce(&mut Cmd) -> io::Result<R>,
    {
        self.configure_and_run_inner(cmd, f, &["CARGO_MAKEFLAGS", "MAKEFLAGS", "MFLAGS"])
    }

    fn configure_and_run_inner<Cmd, F, R>(&self, mut cmd: Cmd, f: F, envs: &[&str]) -> io::Result<R>
    where
        Cmd: Command,
        F: FnOnce(&mut Cmd) -> io::Result<R>,
    {
        #[cfg(unix)]
        self.0.inner.pre_run(&mut cmd);

        let arg = self.0.inner.string_arg();
        // Older implementations of make use `--jobserver-fds` and newer
        // implementations use `--jobserver-auth`, pass both to try to catch
        // both implementations.
        let value = format!("-j --jobserver-fds={0} --jobserver-auth={0}", arg);

        let mut cmd = setup_envs(cmd, envs, ffi::OsStr::new(&value));

        f(&mut cmd)
    }

    /// Same as [`Client::configure_and_run`] except that it tries to pass
    /// `--jobserver-auth=fifo:/path/to/fifo` to pass path to fifo instead of
    /// fds and it does not pass `--jobserver-fds=r,w` on unix and will
    /// fallback to [`Client::configure_and_run`] if the client is not
    /// created using [`Client::new_with_fifo`] or [`Client::from_env`]
    /// with `CARGO_MAKEFLAGS`/`MAKEFLAGS`/`MFLAGS` containing
    /// `--jobserver-auth=fifo:/path/to/fifo`.
    ///
    /// On windows, [`Client`] always uses a named semaphore and on wasm,
    /// such API is not supported since spawning processes is not supported
    /// by wasm yet.
    ///
    /// Using this function will break backwards compatibility for
    /// some programs, e.g. make < `4.4`.
    ///
    /// Using this method does provide better performance since it doesn't need
    /// to call [`Command::pre_run`] to register a callback to run in the new
    /// process and thus can use vfork + exec for better performance.
    pub fn configure_and_run_with_fifo<Cmd, F, R>(&self, cmd: Cmd, f: F) -> io::Result<R>
    where
        Cmd: Command,
        F: FnOnce(&mut Cmd) -> io::Result<R>,
    {
        self.configure_and_run_with_fifo_inner(cmd, f, &["CARGO_MAKEFLAGS"])
    }

    /// Same as [`Client::configure_and_run_with_fifo`] except that it sets up
    /// environment variables `CARGO_MAKEFLAGS`, `MAKEFLAGS` and `MFLAGS`,
    /// which is used by `cargo` and `make`.
    pub fn configure_make_and_run_with_fifo<Cmd, F, R>(&self, cmd: Cmd, f: F) -> io::Result<R>
    where
        Cmd: Command,
        F: FnOnce(&mut Cmd) -> io::Result<R>,
    {
        self.configure_and_run_with_fifo_inner(cmd, f, &["CARGO_MAKEFLAGS", "MAKEFLAGS", "MFLAGS"])
    }

    fn configure_and_run_with_fifo_inner<Cmd, F, R>(
        &self,
        cmd: Cmd,
        f: F,
        envs: &[&str],
    ) -> io::Result<R>
    where
        Cmd: Command,
        F: FnOnce(&mut Cmd) -> io::Result<R>,
    {
        #[cfg(unix)]
        {
            if let Some(path) = self.0.inner.get_fifo() {
                let path = path.as_os_str();

                let prefix = "-j --jobserver-auth=";

                let mut value = ffi::OsString::with_capacity(prefix.len() + path.len());
                value.push(prefix);
                value.push(path);

                let mut cmd = setup_envs(cmd, envs, &value);

                return f(&mut cmd);
            }
        }

        self.configure_and_run_inner(cmd, f, envs)
    }

    /// Blocks the current thread until a token is acquired.
    ///
    /// This is the same as `acquire`, except that it doesn't return an RAII
    /// helper. If successful the process will need to guarantee that
    /// `release_raw` is called in the future.
    pub fn acquire_raw(&self) -> io::Result<()> {
        self.0.inner.acquire()?;
        Ok(())
    }

    /// Releases a jobserver token back to the original jobserver.
    ///
    /// This is intended to be paired with `acquire_raw` if it was called, but
    /// in some situations it could also be called to relinquish a process's
    /// implicit token temporarily which is then re-acquired later.
    pub fn release_raw(&self) -> io::Result<()> {
        self.0.inner.release(None)?;
        Ok(())
    }

    /// Get [`TryAcquireClient`], which supports non-blocking acquire.
    ///
    /// It would return `Err(IntoTryAcquireClientError::IncompatibleWithOlderMake)`
    /// on unix if an annoymous pipe is used as jobserver.
    ///
    /// Once this function is called, on unix, `O_NONBLOCK` will bes et on jobserver
    /// until the last instance of [`TryAcquireClient`] is dropped.
    pub fn into_try_acquire_client(self) -> Result<TryAcquireClient, IntoTryAcquireClientError> {
        #[cfg(unix)]
        return {
            // Construct `TryAcquireClient` here, in case `set_nonblocking`
            // failed, its dtor would set it back to blocking.
            let client = TryAcquireClient(self);

            {
                let mut active_try_acquire_client_count =
                    client.0 .0.acitve_try_acquire_client_count();
                *active_try_acquire_client_count += 1;

                if *active_try_acquire_client_count == 1 {
                    client.0 .0.inner.set_nonblocking()?;
                }
            }

            if client.0 .0.inner.is_try_acquire_safe() {
                Ok(client)
            } else {
                Err(IntoTryAcquireClientError::IncompatibleWithOlderMake(client))
            }
        };

        #[cfg(not(unix))]
        return Ok(TryAcquireClient(self));
    }
}

/// An acquired token from a jobserver.
///
/// This token will be released back to the jobserver when it is dropped and
/// otherwise represents the ability to spawn off another thread of work.
#[derive(Debug)]
pub struct Acquired {
    client: Option<Arc<ClientInner>>,
    data: imp::Acquired,
}

impl Acquired {
    fn new(client: &Client, data: imp::Acquired) -> Self {
        Self {
            client: Some(client.0.clone()),
            data,
        }
    }

    /// This drops the `Acquired` token without releasing the associated token.
    ///
    /// This is not generally useful, but can be helpful if you do not have the
    /// ability to store an Acquired token but need to not yet release it.
    ///
    /// You'll typically want to follow this up with a call to `release_raw` or
    /// similar to actually release the token later on.
    pub fn drop_without_releasing(mut self) {
        self.client = None;
    }
}

impl Drop for Acquired {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            drop(client.inner.release(Some(&self.data)));
        }
    }
}

/// Possible errors for [`Client::into_try_acquire_client`]
#[derive(Debug)]
pub enum IntoTryAcquireClientError {
    /// The jobserver uses annoymous pipe, [`TryAcquireClient::try_acquire`]
    /// requires setting `O_NONBLOCK`, since annoymous pipe is passed by fd,
    /// it will affect all processes using the annoymous pipe,
    /// which will break make < `4.4`.
    ///
    /// If you know that no make < `4.4` is spawned when [`TryAcquireClient`],
    /// then you can simply unwrap this error and continue.
    #[cfg(unix)]
    IncompatibleWithOlderMake(TryAcquireClient),

    /// Setting `O_NONBLOCK` failed.
    #[cfg(unix)]
    IoError(io::Error),
}

impl fmt::Display for IntoTryAcquireClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            #[cfg(unix)]
            Self::IncompatibleWithOlderMake(_) => f.write_str(
                r#"The jobserver uses annoymous pipe, `TryAcquireClient` will set `O_NONBLOCK`,
since annoymous pipe is passed by fd, it will affect all processes using the annoymous pipe,
which will break make < `4.4`."#,
            ),

            #[cfg(unix)]
            Self::IoError(io_error) => write!(f, "io error: {}", io_error),

            _ => unreachable!(),
        }
    }
}

#[cfg(unix)]
impl From<io::Error> for IntoTryAcquireClientError {
    fn from(io_error: io::Error) -> Self {
        Self::IoError(io_error)
    }
}

impl StdError for IntoTryAcquireClientError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            #[cfg(unix)]
            Self::IoError(io_error) => Some(io_error),
            _ => None,
        }
    }
}

/// Extension of [`Client`] that supports non-blocking acquire.
#[derive(Debug, derive_destructure2::destructure)]
pub struct TryAcquireClient(Client);

impl ops::Deref for TryAcquireClient {
    type Target = Client;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl TryAcquireClient {
    /// Similar to [`Client::acquire`], but returns `Ok(None)`
    /// instead of bocking, if there is no token available.
    pub fn try_acquire(&self) -> io::Result<Option<Acquired>> {
        match self.0 .0.inner.try_acquire() {
            Ok(Some(data)) => Ok(Some(Acquired::new(&self.0, data))),
            Ok(None) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Similar to [`Client::acquire_raw`], but returns `Ok(None)`
    /// instead of blocking, if there is no token available.
    pub fn try_acquire_raw(&self) -> io::Result<Option<()>> {
        match self.0 .0.inner.try_acquire() {
            Ok(Some(_)) => Ok(Some(())),
            Ok(None) => Ok(None),
            Err(err) => Err(err),
        }
    }

    #[cfg(unix)]
    fn cleanup(&self) -> io::Result<()> {
        let mut active_try_acquire_client_count = self.0 .0.acitve_try_acquire_client_count();
        *active_try_acquire_client_count -= 1;

        if *active_try_acquire_client_count == 0 {
            self.0 .0.inner.set_blocking()?;
        }

        Ok(())
    }

    /// Get back to [`Client`], return `Err` if clearing `O_NONBLOCK` fails.
    pub fn into_inner(self) -> io::Result<Client> {
        #[cfg(unix)]
        self.cleanup()?;

        Ok(self.destructure().0)
    }
}

impl Drop for TryAcquireClient {
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = self.cleanup();
    }
}

#[cfg(unix)]
impl std::os::unix::prelude::AsRawFd for TryAcquireClient {
    fn as_raw_fd(&self) -> std::os::unix::prelude::RawFd {
        self.0 .0.inner.get_read_fd()
    }
}

#[cfg(any(unix, windows))]
trait GenRandom {
    fn new_random() -> io::Result<Self>
    where Self: Sized;
}

#[cfg(any(unix, windows))]
impl GenRandom for u128 {
    fn new_random() -> io::Result<Self> {
        use std::mem::{MaybeUninit, transmute_copy};
        
        const UNINIT_BYTE: MaybeUninit<u8> = MaybeUninit::<u8>::uninit();
        let mut uninit_bytes = [UNINIT_BYTE; 16];
            
        getrandom::fill_uninit(&mut uninit_bytes)?;

        Ok(u128::from_ne_bytes(unsafe { transmute_copy(&uninit_bytes) }))
    }
}
