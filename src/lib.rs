#![cfg(target_os = "linux")]
//! EventFD binding
//!
//! This crate implements a simple binding for Linux eventfd(). See
//! eventfd(2) for specific details of behaviour.

extern crate nix;

pub use nix::sys::eventfd::{EventFdFlag, EFD_CLOEXEC, EFD_NONBLOCK, EFD_SEMAPHORE};
use nix::sys::eventfd::eventfd;
use nix::unistd::{dup, close, write, read};

use std::io;
use std::os::unix::io::{AsRawFd,RawFd};
use std::thread;
use std::sync::mpsc;
use std::mem;

pub struct EventFD {
    fd: RawFd,
    flags: EventFdFlag,
}

unsafe impl Send for EventFD {}
unsafe impl Sync for EventFD {}

impl EventFD {
    /// Create a new EventFD. Flags is the bitwise OR of EFD_* constants, or 0 for no flags.
    /// The underlying file descriptor is closed when the EventFD instance's lifetime ends.
    ///
    /// TODO: work out how to integrate this FD into the wider world
    /// of fds. There's currently no way to poll/select on the fd.
    pub fn new(initval: usize, flags: EventFdFlag) -> io::Result<EventFD> {
        Ok(EventFD { fd: try!(eventfd(initval, flags)), flags: flags })
    }

    /// Read the current value of the eventfd. This will block until
    /// the value is non-zero. In semaphore mode this will only ever
    /// decrement the count by 1 and return 1; otherwise it atomically
    /// returns the current value and sets it to zero.
    pub fn read(&self) -> io::Result<u64> {
        let mut buf = [0u8; 8];
        let _ = try!(read(self.fd, &mut buf));
        let val = unsafe { mem::transmute(buf) };
        Ok(val)
    }

    /// Add to the current value. Blocks if the value would wrap u64.
    pub fn write(&self, val: u64) -> io::Result<()> {
        let buf: [u8; 8] = unsafe { mem::transmute(val) };
        try!(write(self.fd, &buf));
        Ok(())
    }

    /// Return a stream of events.
    ///
    /// The channel has a synchronous sender because there's no point in building up a queue of
    /// events; if this task blocks on send, the event state will still update.
    ///
    /// The task will exit if the receiver end is shut down.
    ///
    /// This will be a CPU-spin loop if the EventFD is created non-blocking.
    ///
    /// XXX FIXME This has no way of terminating except if the other end closes the connection, and
    /// only then if we're not blocked in the read()...
    pub fn events(&self) -> mpsc::Receiver<u64> {
        let (tx, rx) = mpsc::sync_channel(1);
        let c = self.clone();

        thread::spawn(move || {
            loop {
                match c.read() {
                    Ok(v) => match tx.send(v) {
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
    fn as_raw_fd(&self) -> RawFd {
        self.fd as RawFd
    }
}

impl Drop for EventFD {
    fn drop(&mut self) {
        let _ = close(self.fd);
    }
}

/// Construct a linked clone of an existing EventFD. Once created, the
/// new instance interacts with the original in a way that's
/// indistinguishable from the original.
impl Clone for EventFD {
    fn clone(&self) -> EventFD {
        EventFD { fd: dup(self.fd).unwrap(), flags: self.flags }
    }
}

#[cfg(test)]
mod test {
    extern crate std;
    use super::{EventFdFlag, EventFD, EFD_SEMAPHORE, EFD_NONBLOCK};
    use std::thread;

    #[test]
    fn test_basic() {
        let (tx,rx) = std::sync::mpsc::channel();
        let efd = match EventFD::new(10, EventFdFlag::empty()) {
            Err(e) => panic!("new failed {}", e),
            Ok(fd) => fd,
        };
        let cefd = efd.clone();

        assert_eq!(efd.read().unwrap(), 10);

        thread::spawn(move || {
            assert_eq!(cefd.read().unwrap(), 7);
            assert_eq!(cefd.write(1).unwrap(), ());
            assert_eq!(cefd.write(2).unwrap(), ());
            assert!(tx.send(()).is_ok());
        });

        assert_eq!(efd.write(7).unwrap(), ());
        let _ = rx.recv();
        assert_eq!(efd.read().unwrap(), 3);
    }

    #[test]
    fn test_sema() {
        let efd = match EventFD::new(0, EFD_SEMAPHORE | EFD_NONBLOCK) {
            Err(e) => panic!("new failed {}", e),
            Ok(fd) => fd,
        };

        match efd.read() {
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => (), // ok
            Err(e) => panic!("unexpected error {}", e),
            Ok(v) => panic!("unexpected success {}", v),
        }

        assert_eq!(efd.write(5).unwrap(), ());

        assert_eq!(efd.read().unwrap(), 1);
        assert_eq!(efd.read().unwrap(), 1);
        assert_eq!(efd.read().unwrap(), 1);
        assert_eq!(efd.read().unwrap(), 1);
        assert_eq!(efd.read().unwrap(), 1);
        match efd.read() {
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => (), // ok
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
        let (tx,rx) = std::sync::mpsc::channel();
        let efd = match EventFD::new(10, EventFdFlag::empty()) {
            Err(e) => panic!("new failed {}", e),
            Ok(fd) => fd,
        };

        assert_eq!(efd.write(1).unwrap(), ());
        assert!(tx.send(efd).is_ok());

        let t = thread::spawn(move || {
            let efd = rx.recv().unwrap();
            assert_eq!(efd.read().unwrap(), 11)
        }).join();

        match t {
            Ok(_) => println!("ok"),
            Err(_) => panic!("failed"),
        }
    }
}
