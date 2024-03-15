use std::{
    borrow::Cow,
    convert::TryInto,
    ffi::OsStr,
    fmt::Write as _,
    fs::{self, File},
    io::{self, Read, Write},
    mem::{ManuallyDrop, MaybeUninit},
    os::unix::{ffi::OsStrExt, prelude::*},
    path::{Path, PathBuf},
};

use getrandom::getrandom;
use libc::c_int;

use crate::Command;

#[derive(Debug)]
pub struct Client {
    /// This fd is set to be nonblocking
    read: File,
    /// This fd is set to be blocking
    write: File,
    /// Path to the named fifo if any
    path: Option<Box<Path>>,
    /// If the Client owns the fifo, then we should remove it on drop.
    owns_fifo: bool,
}

#[derive(Debug)]
pub struct Acquired {
    byte: u8,
}

impl Client {
    pub fn new(limit: usize) -> io::Result<Self> {
        // Create nonblocking and cloexec pipes
        let pipes = create_pipe()?;

        let client = unsafe { Self::from_fds(pipes[0], pipes[1]) };

        client.init(limit)?;

        Ok(client)
    }

    pub fn new_fifo(limit: usize) -> io::Result<Self> {
        // Try a bunch of random file name in /tmp until we get a unique one,
        // but don't try for too long.
        let prefix = "/tmp/__rust_jobslot_fifo_";

        let mut name = String::with_capacity(
            prefix.len() +
            // 32B for the max size of u128
            32 +
            // 1B for the null byte
            1,
        );
        name.push_str(prefix);

        for _ in 0..100 {
            let mut bytes = [0; 16];
            getrandom(&mut bytes)?;

            write!(&mut name, "{:x}\0", u128::from_ne_bytes(bytes)).unwrap();

            let res = cvt(unsafe {
                libc::mkfifo(name.as_ptr() as *const _, libc::S_IRUSR | libc::S_IWUSR)
            });

            match res {
                Ok(_) => {
                    name.pop(); // chop off the trailing null
                    let name = PathBuf::from(name);

                    let file = open_file_rw(&name)?;

                    let client = Self {
                        read: file.try_clone()?,
                        write: file,
                        path: Some(name.into_boxed_path()),
                        owns_fifo: true,
                    };

                    client.init(limit)?;

                    return Ok(client);
                }
                Err(err) => {
                    if err.kind() == io::ErrorKind::AlreadyExists {
                        name.truncate(prefix.len());
                        continue;
                    } else {
                        return Err(err);
                    }
                }
            }
        }

        Err(io::Error::new(
            io::ErrorKind::Other,
            "failed to find a unique name for a semaphore",
        ))
    }

    fn init(&self, mut limit: usize) -> io::Result<()> {
        // I don't think the character written here matters, but I could be
        // wrong!
        const BUFFER: [u8; 128] = [b'|'; 128];

        while limit > 0 {
            let n = limit.min(BUFFER.len());

            // Use nonblocking write here so that if the pipe
            // would block, then return err instead of blocking
            // the entire process forever.
            (&self.write).write_all(&BUFFER[..n])?;
            limit -= n;
        }

        Ok(())
    }

    pub unsafe fn open(var: &[u8]) -> Option<Self> {
        if let Some(fifo) = var.strip_prefix(b"fifo:") {
            Self::from_fifo(Path::new(OsStr::from_bytes(fifo)))
        } else {
            Self::from_pipe(OsStr::from_bytes(var).to_str()?)
        }
    }

    /// `--jobserver-auth=fifo:PATH`
    fn from_fifo(path: &Path) -> Option<Self> {
        let file = open_file_rw(path).ok()?;

        if is_pipe(&file)? {
            Some(Self {
                read: file.try_clone().ok()?,
                write: file,
                path: Some(path.into()),
                owns_fifo: false,
            })
        } else {
            None
        }
    }

    /// `--jobserver-auth=fd-for-R,fd-for-W`
    unsafe fn from_pipe(s: &str) -> Option<Self> {
        let (read, write) = s.split_once(',')?;

        let read = read.parse().ok()?;
        let write = write.parse().ok()?;

        let read = ManuallyDrop::new(File::from_raw_fd(read));
        let write = ManuallyDrop::new(File::from_raw_fd(write));

        // Ok so we've got two integers that look like file descriptors, but
        // for extra sanity checking let's see if they actually look like
        // instances of a pipe before we return the client.
        //
        // If we're called from `make` *without* the leading + on our rule
        // then we'll have `MAKEFLAGS` env vars but won't actually have
        // access to the file descriptors.
        match (
            is_pipe(&read),
            is_pipe(&write),
            get_access_mode(&read),
            get_access_mode(&write),
        ) {
            (
                Some(true),
                Some(true),
                Some(libc::O_RDONLY) | Some(libc::O_RDWR),
                Some(libc::O_WRONLY) | Some(libc::O_RDWR),
            ) => {
                // Optimization: Try converting it to a fifo by using /dev/fd
                //
                // On linux, opening `/dev/fd/$fd` returns a fd with a new file description,
                // so we can set `O_NONBLOCK` on it without affecting other processes.
                //
                // On macOS, opening `/dev/fd/$fd` seems to be the same as `File::try_clone`.
                //
                // I tested this on macOS 14 and Linux 6.5.13
                #[cfg(target_os = "linux")]
                if let Ok(Some(jobserver)) =
                    Self::from_fifo(Path::new(&format!("/dev/fd/{}", read.as_raw_fd())))
                {
                    return Ok(Some(jobserver));
                }

                let read = read.try_clone().ok()?;
                let write = write.try_clone().ok()?;

                Some(Self {
                    read,
                    write,
                    path: None,
                    owns_fifo: false,
                })
            }
            _ => None,
        }
    }

    unsafe fn from_fds(read: c_int, write: c_int) -> Self {
        Self {
            read: File::from_raw_fd(read),
            write: File::from_raw_fd(write),
            path: None,
            owns_fifo: false,
        }
    }

    pub fn acquire(&self) -> io::Result<Acquired> {
        loop {
            poll_for_readiness1(self.read.as_raw_fd())?;

            // Ignore EINTR or EAGAIN and keep trying if that happens
            if let Some(token) = self.acquire_allow_interrupts()? {
                return Ok(token);
            }
        }
    }

    /// Waiting for a token in a non-blocking manner, returning `None`
    /// if we're interrupted with EINTR or EAGAIN.
    fn acquire_allow_interrupts(&self) -> io::Result<Option<Acquired>> {
        let mut buf = [0];
        match (&self.read).read(&mut buf) {
            Ok(1) => Ok(Some(Acquired { byte: buf[0] })),
            Ok(_) => Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
            Err(e)
                if e.kind() == io::ErrorKind::Interrupted
                    || e.kind() == io::ErrorKind::WouldBlock =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    pub fn release(&self, data: Option<&Acquired>) -> io::Result<()> {
        // For write to block, this would mean that pipe is full.
        // If all every release are pair with an acquire, then this cannot
        // happen.
        //
        // If it does happen, it is likely a bug in the program using this
        // crate or some other programs that use the same jobserver have a
        // bug in their  code
        //
        // If that turns out to not be the case we'll get an error anyway!
        let byte = data.map(|d| d.byte).unwrap_or(b'+');
        match (&self.write).write(&[byte])? {
            1 => Ok(()),
            _ => Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
        }
    }

    pub fn string_arg(&self) -> Cow<'_, str> {
        Cow::Owned(format!(
            "{},{}",
            self.read.as_raw_fd(),
            self.write.as_raw_fd()
        ))
    }

    pub fn get_fifo(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn pre_run<Cmd>(&self, cmd: &mut Cmd)
    where
        Cmd: Command,
    {
        let read = self.read.as_raw_fd();
        let write = self.write.as_raw_fd();

        let mut fds = Some([read, write]);

        let f = move || {
            // Make sure this function is executed only once,
            // so that the command may be reused with another
            // Client.
            for fd in fds.take().iter().flatten() {
                set_cloexec(*fd, false)?;
            }

            Ok(())
        };

        unsafe { cmd.pre_exec(f) };
    }

    pub fn available(&self) -> io::Result<usize> {
        let mut len = MaybeUninit::<c_int>::uninit();
        cvt(unsafe { libc::ioctl(self.read.as_raw_fd(), libc::FIONREAD, len.as_mut_ptr()) })?;
        Ok(unsafe { len.assume_init() }.try_into().unwrap())
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            if self.owns_fifo {
                fs::remove_file(path).ok();
            }
        }
    }
}

// start of syscalls

/// Return fds that are nonblocking and cloexec
fn create_pipe() -> io::Result<[RawFd; 2]> {
    let mut pipes = [0; 2];

    // Attempt atomically-create-with-cloexec if we can on Linux,
    // detected by using the `syscall` function in `libc` to try to work
    // with as many kernels/glibc implementations as possible.
    #[cfg(target_os = "linux")]
    {
        use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

        static PIPE2_AVAILABLE: AtomicBool = AtomicBool::new(true);
        if PIPE2_AVAILABLE.load(Relaxed) {
            match cvt(unsafe { libc::pipe2(pipes.as_mut_ptr(), libc::O_CLOEXEC) }) {
                Ok(_) => return Ok(pipes),
                Err(err) if err.raw_os_error() != Some(libc::ENOSYS) => return Err(err),

                // err.raw_os_error() == Some(libc::ENOSYS)
                _ => PIPE2_AVAILABLE.store(false, Relaxed),
            }
        }
    }

    cvt(unsafe { libc::pipe(pipes.as_mut_ptr()) })?;

    set_cloexec(pipes[0], true)?;
    set_cloexec(pipes[1], true)?;

    Ok(pipes)
}

fn set_cloexec(fd: c_int, set: bool) -> io::Result<()> {
    // F_GETFD/F_SETFD can only ret/set FD_CLOEXEC
    let flag = if set { libc::FD_CLOEXEC } else { 0 };
    cvt(unsafe { libc::fcntl(fd, libc::F_SETFD, flag) })?;
    Ok(())
}

/*
fn set_fd_flags(fd: c_int, flags: c_int) -> io::Result<()> {
    // Safety: F_SETFL takes one and exactly one c_int flags.
    cvt(unsafe { libc::fcntl(fd, libc::F_SETFL, flags) })?;

    Ok(())
}

fn set_nonblocking(fd: c_int) -> io::Result<()> {
    set_fd_flags(fd, libc::O_NONBLOCK)
}

fn set_blocking(fd: c_int) -> io::Result<()> {
    set_fd_flags(fd, 0)
    }*/

fn cvt(t: c_int) -> io::Result<c_int> {
    if t == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(t)
    }
}

fn cvt_retry_on_interrupt(f: impl Fn() -> c_int) -> io::Result<c_int> {
    loop {
        match cvt(f()) {
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            res => break res,
        }
    }
}

fn is_pipe(file: &File) -> Option<bool> {
    Some(file.metadata().ok()?.file_type().is_fifo())
}

fn get_access_mode(file: &File) -> Option<c_int> {
    let ret = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if ret == -1 {
        return None;
    }

    Some(ret & libc::O_ACCMODE)
}

/// NOTE that this is a blocking syscall, it will block
/// until the fd is ready.
fn poll_for_readiness1(fd: RawFd) -> io::Result<()> {
    let mut fds = [libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    }];

    loop {
        let ret = poll(&mut fds, -1)?;
        if ret != 0 && is_ready(fds[0].revents)? {
            break Ok(());
        }
    }
}

fn poll(fds: &mut [libc::pollfd], timeout: c_int) -> io::Result<c_int> {
    let nfds: libc::nfds_t = fds.len().try_into().unwrap();
    let fds = fds.as_mut_ptr();
    cvt_retry_on_interrupt(move || unsafe { libc::poll(fds, nfds, timeout) })
}

fn is_ready(revents: libc::c_short) -> io::Result<bool> {
    use libc::{POLLERR, POLLHUP, POLLIN, POLLNVAL};

    match revents {
        POLLERR | POLLHUP | POLLIN => Ok(true),
        // This should be very rare
        POLLNVAL => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "fd of is invalid",
        )),
        _ => Ok(false),
    }
}

fn open_file_rw(file: &Path) -> io::Result<File> {
    fs::OpenOptions::new().read(true).write(true).open(file)
}
