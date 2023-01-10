use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::time::Duration;
use std::{io, net};

use reactor::poller::{IoFail, IoType, Poll};

use crate::NetSession;

pub const READ_BUFFER_SIZE: usize = u16::MAX as usize;

pub struct Tunnel<S: NetSession> {
    listener: net::TcpListener,
    session: S,
}

impl<S: NetSession> Tunnel<S> {
    pub fn with(session: S, addr: impl net::ToSocketAddrs) -> Result<Self, (S, io::Error)> {
        let listener = match net::TcpListener::bind(addr) {
            Err(err) => return Err((session, err)),
            Ok(listener) => listener,
        };
        Ok(Self { listener, session })
    }

    pub fn local_addr(&self) -> io::Result<net::SocketAddr> {
        self.listener.local_addr()
    }

    /// # Returns
    ///
    /// Number of bytes which passed through the tunnel
    pub fn tunnel_once<P: Poll>(
        &mut self,
        mut poller: P,
        timeout: Duration,
    ) -> io::Result<(usize, usize)> {
        let (mut stream, _socket_addr) = self.listener.accept()?;

        stream.set_nonblocking(true)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;

        self.session.set_nonblocking(true)?;
        self.session.set_read_timeout(Some(timeout))?;
        self.session.set_write_timeout(Some(timeout))?;

        let int_fd = stream.as_raw_fd();
        let ext_fd = self.session.as_raw_fd();
        poller.register(&int_fd, IoType::read_only());
        poller.register(&ext_fd, IoType::read_only());

        let mut in_buf = VecDeque::<u8>::new();
        let mut out_buf = VecDeque::<u8>::new();

        let mut in_count = 0usize;
        let mut out_count = 0usize;

        let mut buf = [0u8; READ_BUFFER_SIZE];

        macro_rules! handle {
            ($call:expr, |$var:ident| $expr:expr) => {
                match $call {
                    Ok(0) => return Ok((in_count, out_count)),
                    Ok($var) => $expr,
                    Err(err) => return Err(err),
                }
            };
        }

        loop {
            // Blocking
            let count = poller.poll(Some(timeout))?;
            if count > 0 {
                return Err(io::ErrorKind::TimedOut.into());
            }
            while let Some((fd, res)) = poller.next() {
                let ev = match res {
                    Ok(ev) => ev,
                    Err(IoFail::Connectivity(_)) => return Ok((in_count, out_count)),
                    Err(IoFail::Os(_)) => return Err(io::ErrorKind::BrokenPipe.into()),
                };
                if fd == int_fd {
                    if ev.write {
                        handle!(stream.write(in_buf.make_contiguous()), |written| {
                            stream.flush()?;
                            in_buf.drain(..written);
                            in_count += written;
                        });
                    }
                    if ev.read {
                        handle!(stream.read(&mut buf), |read| {
                            out_buf.extend(&buf[..read]);
                        });
                    }
                } else if fd == ext_fd {
                    if ev.write {
                        handle!(self.session.write(out_buf.make_contiguous()), |written| {
                            self.session.flush()?;
                            out_buf.drain(..written);
                            out_count += written;
                        });
                    }
                    if ev.read {
                        handle!(self.session.read(&mut buf), |read| {
                            in_buf.extend(&buf[..read]);
                        });
                    }
                }
            }
        }
    }

    pub fn into_session(self) -> S {
        self.session
    }
}