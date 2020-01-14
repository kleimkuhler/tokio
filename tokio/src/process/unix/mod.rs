//! Unix handling of child processes
//!
//! Right now the only "fancy" thing about this is how we implement the
//! `Future` implementation on `Child` to get the exit status. Unix offers
//! no way to register a child with epoll, and the only real way to get a
//! notification when a process exits is the SIGCHLD signal.
//!
//! Signal handling in general is *super* hairy and complicated, and it's even
//! more complicated here with the fact that signals are coalesced, so we may
//! not get a SIGCHLD-per-child.
//!
//! Our best approximation here is to check *all spawned processes* for all
//! SIGCHLD signals received. To do that we create a `Signal`, implemented in
//! the `tokio-net` crate, which is a stream over signals being received.
//!
//! Later when we poll the process's exit status we simply check to see if a
//! SIGCHLD has happened since we last checked, and while that returns "yes" we
//! keep trying.
//!
//! Note that this means that this isn't really scalable, but then again
//! processes in general aren't scalable (e.g. millions) so it shouldn't be that
//! bad in theory...

mod orphan;
use orphan::{OrphanQueue, OrphanQueueImpl, Wait};

mod reap;
use reap::Reaper;

use crate::io::IoResource;
use crate::process::kill::Kill;
use crate::process::SpawnedChild;
use crate::signal::unix::{signal, Signal, SignalKind};

use mio::event::Source;
use mio::unix::SourceFd;
use mio::{Interest, Registry, Token};
use std::fmt;
use std::future::Future;
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::pin::Pin;
use std::process::ExitStatus;
use std::task::Context;
use std::task::Poll;

impl Wait for std::process::Child {
    fn id(&self) -> u32 {
        self.id()
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.try_wait()
    }
}

impl Kill for std::process::Child {
    fn kill(&mut self) -> io::Result<()> {
        self.kill()
    }
}

lazy_static::lazy_static! {
    static ref ORPHAN_QUEUE: OrphanQueueImpl<std::process::Child> = OrphanQueueImpl::new();
}

struct GlobalOrphanQueue;

impl fmt::Debug for GlobalOrphanQueue {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        ORPHAN_QUEUE.fmt(fmt)
    }
}

impl OrphanQueue<std::process::Child> for GlobalOrphanQueue {
    fn push_orphan(&self, orphan: std::process::Child) {
        ORPHAN_QUEUE.push_orphan(orphan)
    }

    fn reap_orphans(&self) {
        ORPHAN_QUEUE.reap_orphans()
    }
}

#[must_use = "futures do nothing unless polled"]
pub(crate) struct Child {
    inner: Reaper<std::process::Child, GlobalOrphanQueue, Signal>,
}

impl fmt::Debug for Child {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("Child")
            .field("pid", &self.inner.id())
            .finish()
    }
}

pub(crate) fn spawn_child(cmd: &mut std::process::Command) -> io::Result<SpawnedChild> {
    let mut child = cmd.spawn()?;
    let stdin = stdio(child.stdin.take())?;
    let stdout = stdio(child.stdout.take())?;
    let stderr = stdio(child.stderr.take())?;

    let signal = signal(SignalKind::child())?;

    Ok(SpawnedChild {
        child: Child {
            inner: Reaper::new(child, GlobalOrphanQueue, signal),
        },
        stdin,
        stdout,
        stderr,
    })
}

impl Child {
    pub(crate) fn id(&self) -> u32 {
        self.inner.id()
    }
}

impl Kill for Child {
    fn kill(&mut self) -> io::Result<()> {
        self.inner.kill()
    }
}

impl Future for Child {
    type Output = io::Result<ExitStatus>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.inner).poll(cx)
    }
}

#[derive(Debug)]
pub(crate) struct Fd<T> {
    inner: T,
}

impl<T> io::Read for Fd<T>
where
    T: io::Read,
{
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.inner.read(bytes)
    }
}

impl<T> io::Write for Fd<T>
where
    T: io::Write,
{
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.inner.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<T> AsRawFd for Fd<T>
where
    T: AsRawFd,
{
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

impl<T> Source for Fd<T>
where
    T: AsRawFd,
{
    fn register(
        &mut self,
        registry: &Registry,
        token: Token,
        interest: Interest,
    ) -> io::Result<()> {
        SourceFd(&self.as_raw_fd()).register(registry, token, interest)
    }

    fn reregister(
        &mut self,
        registry: &Registry,
        token: Token,
        interest: Interest,
    ) -> io::Result<()> {
        SourceFd(&self.as_raw_fd()).reregister(registry, token, interest)
    }

    fn deregister(&mut self, registry: &Registry) -> io::Result<()> {
        SourceFd(&self.as_raw_fd()).deregister(registry)
    }
}

pub(crate) type ChildStdin = IoResource<Fd<std::process::ChildStdin>>;
pub(crate) type ChildStdout = IoResource<Fd<std::process::ChildStdout>>;
pub(crate) type ChildStderr = IoResource<Fd<std::process::ChildStderr>>;

fn stdio<T>(option: Option<T>) -> io::Result<Option<IoResource<Fd<T>>>>
where
    T: AsRawFd,
{
    let io = match option {
        Some(io) => io,
        None => return Ok(None),
    };

    // Set the fd to nonblocking before we pass it to the event loop
    unsafe {
        let fd = io.as_raw_fd();
        let r = libc::fcntl(fd, libc::F_GETFL);
        if r == -1 {
            return Err(io::Error::last_os_error());
        }
        let r = libc::fcntl(fd, libc::F_SETFL, r | libc::O_NONBLOCK);
        if r == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(Some(IoResource::new(Fd { inner: io })?))
}
