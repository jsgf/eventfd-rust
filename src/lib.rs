//! EventFD binding
//!
//! This crate implements a simple binding for Linux eventfd(). See
//! eventfd(2) for specific details of behaviour.

extern crate libc;

use libc::{c_int, c_uint, c_void};
use std::io::{IoResult, IoError};
use std::os::unix::{AsRawFd,Fd};

#[deriving(Send)]
#[deriving(Sync)]
pub struct EventFD {
    fd: uint,
    flags: uint,
}

/// Construct a semaphore-style EventFD.
pub const EFD_SEMAPHORE: uint = 0x00001; // 00000001

/// In non-blocking mode, reads and writes will return IoError
/// containing EAGAIN rather than blocking.
pub const EFD_NONBLOCK: uint  = 0x00800; // 00004000

/// Set the close-on-exec flag on the eventfd.
pub const EFD_CLOEXEC: uint   = 0x80000; // 02000000

impl EventFD {
    /// Create a new EventFD. Flags is the bitwise OR of EFD_* constants, or 0 for no flags.
    /// The underlying file descriptor is closed when the EventFD instance's lifetime ends.
    ///
    /// TODO: work out how to integrate this FD into the wider world
    /// of fds. There's currently no way to poll/select on the fd.
    pub fn new(initval: uint, flags: uint) -> IoResult<EventFD> {
        let r = unsafe { eventfd(initval as u32, flags as u32) };

        if r < 0 {
            Err(IoError::last_error())
        } else {
            Ok(EventFD { fd: r as uint, flags: flags })
        }
    }

    /// Read the current value of the eventfd. This will block until
    /// the value is non-zero. In semaphore mode this will only ever
    /// decrement the count by 1 and return 1; otherwise it atomically
    /// returns the current value and sets it to zero.
    pub fn read(&self) -> IoResult<u64> {
        let mut ret : u64 = 0;
        let r = unsafe {
            let ptr : *mut u64 = &mut ret;
            ::libc::read(self.fd as c_int, ptr as *mut c_void, std::mem::size_of_val(&ret) as u64)
        };

        if r < 0 {
            Err(IoError::last_error())
        } else {
            Ok(ret)
        }
    }

    /// Add to the current value. Blocks if the value would wrap u64.
    pub fn write(&self, val: u64) -> IoResult<()> {
        let r = unsafe {
            let ptr : *const u64 = &val;
            ::libc::write(self.fd as c_int, ptr as *const c_void, std::mem::size_of_val(&val) as u64)
        };

        if r < 0 {
            Err(IoError::last_error())
        } else {
            Ok(())
        }
    }

    /// Return a stream of events. The channel has a synchronous
    /// sender because there's no point in building up a queue of
    /// events; if this task blocks on send, the event state will
    /// still update.
    ///
    /// The task will exit if the receiver end is shut down.
    ///
    /// This will be a CPU-spin loop if the EventFD is created
    /// non-blocking.
    ///
    /// XXX FIXME This has no way of terminating except if the other
    /// end closes the connection, and only then if we're not blocked
    /// in the read()...
    pub fn events(&self) -> std::comm::Receiver<u64> {
        let (tx, rx) = std::comm::sync_channel(1);
        let c = self.clone();

        spawn(proc() {
            loop {
                match c.read() {
                    Ok(v) => match tx.send_opt(v) {
                        Ok(_) => (),
                        Err(_) => break,
                    },
                    Err(e) => panic!("read failed: {}", e),
                }
            }
        });

        rx
    }
}

impl AsRawFd for EventFD {
    /// Return the raw underlying fd. The caller must make sure self's
    /// lifetime is longer than any users of the fd.
    fn as_raw_fd(&self) -> Fd {
        self.fd as Fd
    }
}

impl Drop for EventFD {
    fn drop(&mut self) {
        let r = unsafe { ::libc::close(self.fd as c_int) };
        if r < 0 {
            panic!("eventfd close {} failed", self.fd)
        }
    }
}

/// Construct a linked clone of an existing EventFD. Once created, the
/// new instance interacts with the original in a way that's
/// indistinguishable from the original.
impl Clone for EventFD {
    fn clone(&self) -> EventFD {
        let r = unsafe { ::libc::dup(self.fd as c_int) };
        if r < 0 {
            panic!("EventFD clone dup failed: {}", IoError::last_error())
        }

        EventFD { fd: r as uint, flags: self.flags }
    }
}

#[link(name = "c")]
extern "C" {
    fn eventfd(initval: c_uint, flags: c_uint) -> c_int;
}


#[cfg(test)]
mod test {
    extern crate std;
    use super::{EventFD, EFD_SEMAPHORE, EFD_NONBLOCK};

    #[test]
    fn test_basic() {
        let (tx,rx) = std::comm::channel();
        let efd = match EventFD::new(10, 0) {
            Err(e) => panic!("new failed {}", e),
            Ok(fd) => fd,
        };
        let cefd = efd.clone();

        assert_eq!(efd.read(), Ok(10));

        spawn(proc() {
            assert_eq!(cefd.read(), Ok(7));
            assert_eq!(cefd.write(1), Ok(()));
            assert_eq!(cefd.write(2), Ok(()));
            tx.send(());
        });

        assert_eq!(efd.write(7), Ok(()));
        rx.recv();
        assert_eq!(efd.read(), Ok(3));
    }

    #[test]
    fn test_sema() {
        let efd = match EventFD::new(0, EFD_SEMAPHORE | EFD_NONBLOCK) {
            Err(e) => panic!("new failed {}", e),
            Ok(fd) => fd,
        };

        match efd.read() {
            Err(ref e) if e.kind == std::io::ResourceUnavailable => (), // ok
            Err(e) => panic!("unexpected error {}", e),
            Ok(v) => panic!("unexpected success {}", v),
        }

        assert_eq!(efd.write(5), Ok(()));

        assert_eq!(efd.read(), Ok(1));
        assert_eq!(efd.read(), Ok(1));
        assert_eq!(efd.read(), Ok(1));
        assert_eq!(efd.read(), Ok(1));
        assert_eq!(efd.read(), Ok(1));
        match efd.read() {
            Err(ref e) if e.kind == std::io::ResourceUnavailable => (), // ok
            Err(e) => panic!("unexpected error {}", e),
            Ok(v) => panic!("unexpected success {}", v),
        }
    }

    #[test]
    fn test_stream() {
        let efd = match EventFD::new(11, EFD_SEMAPHORE) {
            Err(e) => panic!("new failed {}", e),
            Ok(fd) => fd,
        };
        let mut count = 0;

        // only take 10 of 11 so the stream task doesn't block in read and hang the test
        for v in efd.events().iter().take(10) {
            assert_eq!(v, 1);
            count += v;
        }

        assert_eq!(count, 10)
    }

    #[test]
    fn test_chan() {
        let (tx,rx) = std::comm::channel();
        let efd = match EventFD::new(10, 0) {
            Err(e) => panic!("new failed {}", e),
            Ok(fd) => fd,
        };

        assert_eq!(efd.write(1), Ok(()));
        tx.send(efd);

        let t = std::task::try(proc() {
            let efd = rx.recv();
            assert_eq!(efd.read(), Ok(11))
        });

        match t {
            Ok(_) => println!("ok"),
            Err(_) => panic!("failed"),
        }
    }
}
