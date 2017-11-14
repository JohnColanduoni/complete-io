use super::{Epoll, Handle, FuturesRegistration, FutureInterest, EventMask};
use ::io::{AsyncRead, AsyncWrite, AsRegistrar};
use ::net::{TcpStream as GenTcpStream};

use std::{io, mem, slice};
use std::sync::Arc;
use std::net::{TcpStream as StdTcpStream, TcpListener as StdTcpListener, UdpSocket as StdUdpSocket, SocketAddr};
use std::os::unix::prelude::*;

use futures::prelude::*;
use bytes::{Bytes, BytesMut};
use libc;
use net2::{TcpBuilder, TcpStreamExt};

#[derive(Clone)]
pub struct TcpListener {
    inner: Arc<_TcpListener>,
}

struct _TcpListener {
    std: StdTcpListener,
    registration: FuturesRegistration,
    handle: Handle,
}

impl ::net::TcpListener for TcpListener {
    type EventRegistrar = Epoll;
    type TcpStream = TcpStream;
    type Accept = Accept;

    fn from_listener<R: AsRegistrar<Self::EventRegistrar>>(listener: StdTcpListener, handle: &R) -> io::Result<Self> {
        let handle = handle.as_registrar();
        let registration = FuturesRegistration::new(handle, &listener)?;
        listener.set_nonblocking(true)?;
        Ok(TcpListener { inner: Arc::new(_TcpListener {
            std: listener,
            registration,
            handle: handle.clone(),
        })})
    }

    fn accept(&self) -> Self::Accept {
        Accept { state: AcceptState::Pending(self.clone(), None) }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.std.local_addr()
    }
}

pub struct Accept {
    state: AcceptState,
}

enum AcceptState {
    Pending(TcpListener, Option<FutureInterest>),
    None,
}

impl Future for Accept {
    type Item = (TcpStream, SocketAddr);
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, io::Error> {
        match mem::replace(&mut self.state, AcceptState::None) {
            AcceptState::Pending(listener, mut interest) => {
                if let Some(Async::NotReady) = interest.as_ref().map(|x| x.poll()) {
                    self.state = AcceptState::Pending(listener, interest);
                    return Ok(Async::NotReady);
                }

                match listener.inner.std.accept() {
                    Ok((stream, addr)) => {
                        let stream = TcpStream::from_stream(stream, &listener.inner.handle)?;
                        Ok(Async::Ready((stream, addr)))
                    },
                    Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {
                        if let Some(ref interest) = interest {
                            interest.register();
                        } else {
                            interest = Some(listener.inner.registration.add_interest(EventMask::READ)?);
                        }
                        self.state = AcceptState::Pending(listener, interest);
                        Ok(Async::NotReady)
                    },
                    Err(err) => Err(err),
                }
            },
            AcceptState::None => panic!("future has already completed"),
        }
    }
}

#[derive(Clone)]
pub struct TcpStream {
    inner: Arc<_TcpStream>,
}

struct _TcpStream {
    std: StdTcpStream,
    registration: FuturesRegistration,
}

impl ::net::TcpStream for TcpStream {
    type EventRegistrar = Epoll;
    type Connect = Connect;

    fn connect<R: AsRegistrar<Self::EventRegistrar>>(addr: &SocketAddr, handle: &R) -> Self::Connect {
        match (|| {
            let builder = if addr.is_ipv4() {
                TcpBuilder::new_v4()?
            } else {
                TcpBuilder::new_v6()?
            };
            builder.to_tcp_stream()
        })() {
            Ok(stream) => Self::connect_with(stream, addr, handle),
            Err(err) => Connect { state: ConnectState::Error(err) },
        }
    }

    fn connect_with<R: AsRegistrar<Self::EventRegistrar>>(stream: StdTcpStream, addr: &SocketAddr, handle: &R) -> Self::Connect {
        let state = match Self::from_stream(stream, handle) {
            Ok(stream) => ConnectState::Pending(stream, addr.clone(), None),
            Err(err) => ConnectState::Error(err),
        };
        Connect { state }
    }

    fn from_stream<R: AsRegistrar<Self::EventRegistrar>>(stream: StdTcpStream, handle: &R) -> io::Result<Self> {
        let handle = handle.as_registrar();
        let registration = FuturesRegistration::new(handle, &stream)?;
        stream.set_nonblocking(true)?;
        Ok(TcpStream { inner: Arc::new(_TcpStream {
            std: stream,
            registration,
        })})
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.std.local_addr()
    }
}

impl AsyncRead for TcpStream {
    type Read = TcpRead;

    #[inline]
    fn read(self, buffer: BytesMut) -> Self::Read {
        TcpRead { state: TcpReadState::Pending(self, buffer, None) }
    }
}

impl AsyncWrite for TcpStream {
    type Write = TcpWrite;

    #[inline]
    fn write(self, buffer: Bytes) -> Self::Write {
        TcpWrite { state: TcpWriteState::Pending(self, buffer, None) }
    }
}

pub struct Connect {
    state: ConnectState,
}

enum ConnectState {
    Pending(TcpStream, SocketAddr, Option<FutureInterest>),
    Error(io::Error),
    None,
}

impl Future for Connect {
    type Item = TcpStream;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, io::Error> {
        match mem::replace(&mut self.state, ConnectState::None) {
            ConnectState::Pending(stream, addr, mut interest) => {
                if let Some(Async::NotReady) = interest.as_ref().map(|x| x.poll()) {
                    self.state = ConnectState::Pending(stream, addr, interest);
                    return Ok(Async::NotReady);
                }

                match stream.inner.std.connect(&addr) {
                    Ok(()) => {
                        Ok(Async::Ready(stream))
                    },
                    Err(ref err) if err.kind() == io::ErrorKind::WouldBlock || err.raw_os_error() == Some(libc::EINPROGRESS) => {
                        if let Some(ref interest) = interest {
                            interest.register();
                        } else {
                            interest = Some(stream.inner.registration.add_interest(EventMask::WRITE)?);
                        }
                        self.state = ConnectState::Pending(stream, addr, interest);
                        Ok(Async::NotReady)
                    },
                    Err(err) => Err(err),
                }
            },
            ConnectState::Error(err) => Err(err),
            ConnectState::None => panic!("future has already completed"),
        }
    }
}

pub struct TcpRead {
    state: TcpReadState,
}

enum TcpReadState {
    Pending(TcpStream, BytesMut, Option<FutureInterest>),
    None
}

impl Future for TcpRead {
    type Item = (TcpStream, BytesMut, usize);
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, io::Error> {
        match mem::replace(&mut self.state, TcpReadState::None) {
            TcpReadState::Pending(stream, mut buffer, mut interest) => {
                if let Some(Async::NotReady) = interest.as_ref().map(|x| x.poll()) {
                    self.state = TcpReadState::Pending(stream, buffer, interest);
                    return Ok(Async::NotReady);
                }

                let buffer_old_len = buffer.len();
                let byte_count = match unsafe { libc::recv(stream.inner.std.as_raw_fd(), buffer.as_mut_ptr() as _, buffer.capacity(), 0) } {
                    -1 => {
                        let error = io::Error::last_os_error();
                        if error.kind() == io::ErrorKind::WouldBlock {
                            if let Some(ref interest) = interest {
                                interest.register();
                            } else {
                                interest = Some(stream.inner.registration.add_interest(EventMask::READ)?);
                            }
                            self.state = TcpReadState::Pending(stream, buffer, interest);
                            return Ok(Async::NotReady);
                        }
                        return Err(error);
                    },
                    b => b as usize,
                };

                // Grow the allocated section of the buffer if we wrote past it into the reserved area
                if byte_count > buffer_old_len {
                    unsafe { buffer.set_len(byte_count); }
                }

                Ok(Async::Ready((stream, buffer, byte_count)))
            },
            TcpReadState::None => panic!("future has already completed"),
        }
    }
}

pub struct TcpWrite {
    state: TcpWriteState,
}

enum TcpWriteState {
    Pending(TcpStream, Bytes, Option<FutureInterest>),
    None
}

impl Future for TcpWrite {
    type Item = (TcpStream, Bytes, usize);
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, io::Error> {
        match mem::replace(&mut self.state, TcpWriteState::None) {
            TcpWriteState::Pending(stream, bytes, mut interest) => {
                if let Some(Async::NotReady) = interest.as_ref().map(|x| x.poll()) {
                    self.state = TcpWriteState::Pending(stream, bytes, interest);
                    return Ok(Async::NotReady);
                }

                let byte_count = match unsafe { libc::send(stream.inner.std.as_raw_fd(), bytes.as_ptr() as _, bytes.len(), 0) } {
                    -1 => {
                        let error = io::Error::last_os_error();
                        if error.kind() == io::ErrorKind::WouldBlock {
                            if let Some(ref interest) = interest {
                                interest.register();
                            } else {
                                interest = Some(stream.inner.registration.add_interest(EventMask::WRITE)?);
                            }
                            self.state = TcpWriteState::Pending(stream, bytes, interest);
                            return Ok(Async::NotReady);
                        }
                        return Err(error);
                    },
                    b => b as usize,
                };

                Ok(Async::Ready((stream, bytes, byte_count)))
            },
            TcpWriteState::None => panic!("future has already completed"),
        }
    }
}

#[derive(Clone)]
pub struct UdpSocket {
    inner: Arc<_UdpSocket>,
}

struct _UdpSocket {
    std: StdUdpSocket,
    registration: FuturesRegistration,
}

impl ::net::UdpSocket for UdpSocket {
    type EventRegistrar = Epoll;

    type SendTo = SendTo;
    type RecvFrom = RecvFrom;

    fn from_socket<R: AsRegistrar<Self::EventRegistrar>>(socket: StdUdpSocket, handle: &R) -> io::Result<Self> {
        let handle = handle.as_registrar();
        let registration = FuturesRegistration::new(handle, &socket)?;
        socket.set_nonblocking(true)?;
        Ok(UdpSocket { inner: Arc::new(_UdpSocket {
            std: socket,
            registration,
        })})
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.std.local_addr()
    }

    fn connect(&self, addr: &SocketAddr) -> io::Result<()> {
        self.inner.std.connect(addr)
    }

    fn send_to(self, buffer: Bytes, addr: &SocketAddr) -> SendTo {
        SendTo { state: SendToState::Pending(self, buffer, addr.clone(), None) }
    }

    fn recv_from(self, buffer: BytesMut) -> RecvFrom {
        RecvFrom { state: RecvFromState::Pending(self, buffer, None) }
    }
}

pub struct RecvFrom {
    state: RecvFromState,
}

enum RecvFromState {
    Pending(UdpSocket, BytesMut, Option<FutureInterest>),
    None
}

impl Future for RecvFrom {
    type Item = (UdpSocket, BytesMut, usize, SocketAddr);
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, io::Error> {
        match mem::replace(&mut self.state, RecvFromState::None) {
            RecvFromState::Pending(socket, mut buffer, mut interest) => {
                if let Some(Async::NotReady) = interest.as_ref().map(|x| x.poll()) {
                    self.state = RecvFromState::Pending(socket, buffer, interest);
                    return Ok(Async::NotReady);
                }

                let buffer_old_len = buffer.len();
                match socket.inner.std.recv_from(unsafe { slice::from_raw_parts_mut(buffer.as_mut_ptr(), buffer.capacity()) })  {
                    Ok((byte_count, remote_addr)) => {
                        // Grow the allocated section of the buffer if we wrote past it into the reserved area
                        if byte_count > buffer_old_len {
                            unsafe { buffer.set_len(byte_count); }
                        }
                        Ok(Async::Ready((socket, buffer, byte_count, remote_addr)))
                    },
                    Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {
                        if let Some(ref interest) = interest {
                            interest.register();
                        } else {
                            interest = Some(socket.inner.registration.add_interest(EventMask::READ)?);
                        }
                        self.state = RecvFromState::Pending(socket, buffer, interest);
                        return Ok(Async::NotReady);
                    },
                    Err(err) => Err(err)
                }
            },
            RecvFromState::None => panic!("future has already completed"),
        }
    }
}

pub struct SendTo {
    state: SendToState,
}

enum SendToState {
    Pending(UdpSocket, Bytes, SocketAddr, Option<FutureInterest>),
    None
}

impl Future for SendTo {
    type Item = (UdpSocket, Bytes);
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, io::Error> {
        match mem::replace(&mut self.state, SendToState::None) {
            SendToState::Pending(socket, buffer, addr, mut interest) => {
                if let Some(Async::NotReady) = interest.as_ref().map(|x| x.poll()) {
                    self.state = SendToState::Pending(socket, buffer, addr, interest);
                    return Ok(Async::NotReady);
                }

                match socket.inner.std.send_to(&buffer, &addr)  {
                    Ok(byte_count) => {
                        if byte_count != buffer.len() {
                            return Err(io::ErrorKind::WriteZero.into());
                        }
                        Ok(Async::Ready((socket, buffer)))
                    },
                    Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {
                        if let Some(ref interest) = interest {
                            interest.register();
                        } else {
                            interest = Some(socket.inner.registration.add_interest(EventMask::WRITE)?);
                        }
                        self.state = SendToState::Pending(socket, buffer, addr, interest);
                        return Ok(Async::NotReady);
                    },
                    Err(err) => Err(err)
                }
            },
            SendToState::None => panic!("future has already completed"),
        }
    }
}

impl ::net::NetEventQueue for Epoll {
    type TcpListener = TcpListener;
    type TcpStream = TcpStream;
    type UdpSocket = UdpSocket;
}
