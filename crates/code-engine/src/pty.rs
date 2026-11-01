//! Unix PTYs. Bytes go only to the caller's chosen sink; this module never logs.
use std::{
    fs::File,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::process::CommandExt,
    },
    pin::Pin,
    process::Stdio,
    sync::Arc,
    task::{Context, Poll, ready},
};
use tokio::{
    io::{AsyncRead, ReadBuf, unix::AsyncFd},
    process::Command,
};

pub(crate) struct Pty {
    master: AsyncFd<File>,
    writer: tokio::sync::Mutex<()>,
}
pub(crate) struct Reader(pub Arc<Pty>);
impl Pty {
    pub fn signal_foreground(&self, signal: i32) {
        let group = unsafe { libc::tcgetpgrp(self.master.get_ref().as_raw_fd()) };
        if group > 0 {
            unsafe {
                libc::kill(-group, signal);
            }
        }
    }
    pub fn open(command: &mut Command) -> io::Result<Arc<Self>> {
        let (mut master, mut slave) = (-1, -1);
        let mut size = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: valid output pointers and an initialized winsize; descriptors
        // are immediately placed in owning File values.
        if unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        let master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        for file in [&master, &slave] {
            if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
                return Err(io::Error::last_os_error());
            }
        }
        if unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } == -1 {
            return Err(io::Error::last_os_error());
        }
        command
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        // Only async-signal-safe syscalls after fork. stdio has been installed.
        unsafe {
            command.as_std_mut().pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(Arc::new(Self {
            // SAFETY: master is owned, nonblocking and never replaced or closed
            // while registered; get_ref only performs reads/writes/ioctls.
            master: unsafe { AsyncFd::register(master)? },
            writer: tokio::sync::Mutex::new(()),
        }))
    }
    pub async fn write(&self, mut bytes: &[u8]) -> io::Result<()> {
        let _writer = self.writer.lock().await;
        while !bytes.is_empty() {
            let mut ready = self.master.writable().await?;
            match ready.try_io(|fd| fd.get_ref().write(bytes)) {
                Ok(Ok(0)) => return Err(io::Error::new(io::ErrorKind::WriteZero, "PTY closed")),
                Ok(Ok(n)) => bytes = &bytes[n..],
                Ok(Err(e)) => return Err(e),
                Err(_) => {}
            }
        }
        Ok(())
    }
    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        if cols == 0 || rows == 0 || cols > 1000 || rows > 1000 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "terminal size must be 1..1000",
            ));
        }
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        if unsafe { libc::ioctl(self.master.get_ref().as_raw_fd(), libc::TIOCSWINSZ, &size) } == -1
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
pub(crate) struct ProcessGroup(pub i32);
impl ProcessGroup {
    pub fn signal(&self, signal: i32) {
        unsafe {
            libc::kill(-self.0, signal);
        }
    }
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.signal(libc::SIGKILL);
    }
}
impl AsyncRead for Reader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let mut ready = ready!(self.0.master.poll_read_ready(cx))?;
            match ready.try_io(|fd| fd.get_ref().read(buf.initialize_unfilled())) {
                Ok(Ok(n)) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(e)) if e.raw_os_error() == Some(libc::EIO) => return Poll::Ready(Ok(())),
                Ok(Err(e)) => return Poll::Ready(Err(e)),
                Err(_) => {}
            }
        }
    }
}
