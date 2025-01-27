use std::{
    borrow::Cow,
    convert::TryInto,
    ffi::CString,
    fmt::Write,
    io,
    mem::MaybeUninit,
    num::NonZeroIsize,
    os::windows::io::{AsRawHandle, HandleOrNull, OwnedHandle},
    ptr,
};

use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, FALSE, HANDLE as RawHandle, WAIT_ABANDONED, WAIT_FAILED,
        WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    System::{
        Threading::{
            CreateSemaphoreA, ReleaseSemaphore, WaitForSingleObject, INFINITE,
            SEMAPHORE_MODIFY_STATE, THREAD_SYNCHRONIZE as SYNCHRONIZE,
        },
        WindowsProgramming::OpenSemaphoreA,
    },
};

use crate::{Command, GenRandom};

type LONG = i32;

#[derive(Debug)]
pub struct Client {
    sem: OwnedHandle,
    name: Box<str>,
}

#[derive(Debug)]
pub struct Acquired;

impl Client {
    pub fn new(limit: usize) -> io::Result<Client> {
        let limit: LONG = limit
            .try_into()
            .map_err(|err| io::Error::new(io::ErrorKind::Other, err))?;

        // Note that `limit == 0` is a valid argument above but Windows
        // won't let us create a semaphore with 0 slots available to it. Get
        // `limit == 0` working by creating a semaphore instead with one
        // slot and then immediately acquire it (without ever releaseing it
        // back).
        let create_limit: LONG = if limit == 0 { 1 } else { limit };

        // Try a bunch of random semaphore names until we get a unique one,
        // but don't try for too long.
        let prefix = "__rust_jobslot_semaphore_";

        let mut name = String::with_capacity(
            prefix.len() +
            // 32B for the max size of u128
            32 +
            // 1B for the null byte
            1,
        );
        name.push_str(prefix);

        for _ in 0..100 {
            write!(&mut name, "{}\0", u128::new_random()?).unwrap();

            let res: io::Result<OwnedHandle> = unsafe {
                HandleOrNull::from_raw_handle(CreateSemaphoreA(
                    ptr::null_mut(),
                    create_limit,
                    create_limit,
                    name.as_ptr(),
                ))
            }
            .try_into()
            .map_err(|_| io::Error::last_os_error());

            match res {
                Ok(sem) => {
                    name.pop(); // chop off the trailing nul
                    let client = Client {
                        sem,
                        name: name.into_boxed_str(),
                    };
                    if create_limit != limit {
                        client.acquire()?;
                    }
                    return Ok(client);
                }
                Err(err) => {
                    if err.raw_os_error() == Some(ERROR_ALREADY_EXISTS.try_into().unwrap()) {
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

    pub unsafe fn open(var: &[u8]) -> Option<Client> {
        HandleOrNull::from_raw_handle(OpenSemaphoreA(
            SYNCHRONIZE | SEMAPHORE_MODIFY_STATE,
            FALSE,
            CString::new(var).ok()?.as_bytes().as_ptr(),
        ))
        .try_into()
        .ok()
        .map(|sem| Client {
            sem,
            name: String::from_utf8_lossy(var).into(),
        })
    }

    pub fn acquire(&self) -> io::Result<Acquired> {
        self.acquire_inner(INFINITE).map(|res| {
            res.expect("With timeout set to infinite, WAIT_TIMEOUT should not be returned")
        })
    }

    /// * `timeout` - can be `INFINITE` or 0 or any other number.
    fn acquire_inner(&self, timeout: u32) -> io::Result<Option<Acquired>> {
        let r = unsafe { WaitForSingleObject(self.sem.as_raw_handle(), timeout) };

        match r {
            WAIT_OBJECT_0 => Ok(Some(Acquired)),
            WAIT_TIMEOUT => Ok(None),
            WAIT_FAILED => Err(io::Error::last_os_error()),
            // We believe this should be impossible for a semaphore, but still
            // check the error code just in case it happens.
            WAIT_ABANDONED => Err(io::Error::new(
                io::ErrorKind::Other,
                "Wait on jobserver semaphore returned WAIT_ABANDONED",
            )),
            ret => Err(io::Error::new(
                io::ErrorKind::Other,
                format!(
                    "Unexpected return value `{:#01x}` from WaitForSingleObject",
                    ret
                ),
            )),
        }
    }

    pub fn try_acquire(&self) -> io::Result<Option<Acquired>> {
        self.acquire_inner(0)
    }

    pub fn release(&self, _data: Option<&Acquired>) -> io::Result<()> {
        self.release_inner(None)
    }

    fn release_inner(&self, prev_count: Option<&mut MaybeUninit<LONG>>) -> io::Result<()> {
        // SAFETY: ReleaseSemaphore will write to prev_count is it is Some
        // and release semaphore self.sem by 1.
        let r = unsafe {
            ReleaseSemaphore(
                self.sem.as_raw_handle(),
                1,
                prev_count
                    .map(MaybeUninit::as_mut_ptr)
                    .unwrap_or_else(ptr::null_mut),
            )
        };
        if r != 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub fn string_arg(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.name)
    }

    pub fn available(&self) -> io::Result<usize> {
        // Can't read value of a semaphore on Windows, so
        // try to acquire without sleeping, since we can find out the
        // old value on release.
        if self.acquire_inner(0)?.is_some() {
            let mut prev = MaybeUninit::uninit();
            self.release_inner(Some(&mut prev))?;
            // SAFETY: release_inner has initialized it
            let prev: usize = unsafe { prev.assume_init() }.try_into().unwrap();
            Ok(prev + 1)
        } else {
            Ok(0)
        }
    }
}
