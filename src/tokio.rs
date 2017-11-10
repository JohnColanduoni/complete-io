use ::evloop::{EventLoop, LocalHandle, RemoteHandle, Registrar, AsRegistrar};
use ::net::{NetEventLoop};
use ::io::*;

use std::{mem, io};
use std::sync::Arc;
use std::time::Duration;
use std::io::{Error, Result, Read as IoRead, Write as IoWrite};
use std::net::{SocketAddr, TcpStream as StdTcpStream, TcpListener as StdTcpListener, UdpSocket as StdUdpSocket};

use futures;
use futures::prelude::*;
use bytes::{Bytes, BytesMut};
use tokio_core::reactor::{Core, Handle, Remote, PollEvented};
use mio::net::{TcpStream as MioTcpStream, TcpListener as MioTcpListener, UdpSocket as MioUdpSocket};

impl EventLoop for Core {
    type LocalHandle = Handle;
    type RemoteHandle = Remote;
    type Registrar = Handle;

    fn handle(&self) -> Self::LocalHandle {
        Core::handle(self)
    }

    fn remote(&self) -> Self::RemoteHandle {
        Core::remote(self)
    }

    fn run<F>(&mut self, f: F) -> ::std::result::Result<F::Item, F::Error> where
        F: Future,
    {
        Core::run(self, f)
    }

    fn turn(&mut self, max_wait: Option<Duration>) {
        Core::turn(self, max_wait)
    }
}

impl LocalHandle for Handle {
    type EventLoop = Core;

    fn remote(&self) -> &<Self::EventLoop as EventLoop>::RemoteHandle {
        Handle::remote(self)
    }

    fn spawn<F>(&self, f: F) where
        F: Future<Item = (), Error = ()> + 'static,
    {
        Handle::spawn(self, f)
    }

    fn spawn_fn<F, R>(&self, f: F) where
        F: FnOnce() -> R + 'static,
        R: IntoFuture<Item = (), Error = ()> + 'static,
    {
        Handle::spawn_fn(self, f)
    }
}

impl Registrar for Handle {
    type EventLoop = Core;
}

impl AsRegistrar<Core> for Handle {
    fn as_registrar(&self) -> &Handle { self }
}

impl RemoteHandle for Remote {
    type EventLoop = Core;

    fn local(&self) -> Option<Handle> {
        Remote::handle(self)
    }

    fn spawn_locked<F, R>(&self, f: F) where
        F: FnOnce(&<Self::EventLoop as EventLoop>::LocalHandle) -> R + Send + 'static,
        R: IntoFuture<Item = (), Error = ()>,
        R::Future: 'static
    {
        Remote::spawn(self, f)
    }

    fn spawn<F>(&self, f: F) where
        F: Future<Item = (), Error = ()> + Send + 'static,
    {
        Remote::spawn(self, |_| f)
    }

    fn spawn_fn<F, R>(&self, f: F) where
        F: FnOnce(&<Self::EventLoop as EventLoop>::LocalHandle) -> R + Send + 'static,
        R: IntoFuture<Item = (), Error = ()>,
        R::Future: Send + 'static,
    {
        Remote::spawn(self, f)
    }
}

impl NetEventLoop for Core {
    type TcpStream = TcpStream;
    type TcpListener = TcpListener;
    type UdpSocket = UdpSocket;
}

#[derive(Clone)]
pub struct TcpStream {
    inner: Arc<PollEvented<MioTcpStream>>,
}

#[derive(Clone)]
pub struct TcpListener {
    inner: Arc<PollEvented<MioTcpListener>>,
}

#[derive(Clone)]
pub struct UdpSocket {
    inner: Arc<PollEvented<MioUdpSocket>>,
}

impl ::net::TcpStream for TcpStream {
    type EventLoop = Core;

    type Connect = Connect;

    fn connect<R>(addr: &SocketAddr, handle: &R) -> Self::Connect where
        R: AsRegistrar<Self::EventLoop>,
    {
        let handle = handle.as_registrar();
        let state = match MioTcpStream::connect(addr).and_then(|stream| PollEvented::new(stream, handle)) {
            Ok(stream) => ConnectState::Pending(TcpStream {
                inner: Arc::new(stream)
            }),
            Err(err) => ConnectState::Error(err),
        };
        Connect { state }
    }

    fn from_stream<R>(stream: StdTcpStream, handle: &R) -> Result<Self> where
        R: AsRegistrar<Self::EventLoop>,
    {
        let handle = handle.as_registrar();
        let stream = PollEvented::new(MioTcpStream::from_stream(stream)?, handle)?;
        Ok(TcpStream {
            inner: Arc::new(stream),
        })
    }

    fn local_addr(&self) -> Result<SocketAddr> { self.inner.get_ref().local_addr() }
}

impl AsyncRead for TcpStream {
    type Read = TcpRead;

    fn read(self, buffer: BytesMut) -> TcpRead {
        TcpRead {
            state: TcpReadState::Pending(self.clone(), buffer)
        }
    }
}

impl AsyncWrite for TcpStream {
    type Write = TcpWrite;

    fn write(self, buffer: Bytes) -> TcpWrite {
        TcpWrite {
            state: TcpWriteState::Pending(self.clone(), buffer)
        }
    }
}

impl ::net::TcpListener for TcpListener {
    type EventLoop = Core;
    type TcpStream = TcpStream;

    type Accept = Accept;

    fn from_listener<R>(listener: StdTcpListener, handle: &R) -> Result<Self> where
        R: AsRegistrar<Core>,
    {
        let handle = handle.as_registrar();
        let local_addr = listener.local_addr()?;
        let listener = PollEvented::new(MioTcpListener::from_listener(listener, &local_addr)?, handle)?;
        Ok(TcpListener {
            inner: Arc::new(listener),
        })
    }

    fn accept(&self) -> Self::Accept {
        Accept {
            state: AcceptState::Pending(self.clone()),
        }
    }

    fn local_addr(&self) -> Result<SocketAddr> { self.inner.get_ref().local_addr() }
}

impl ::net::UdpSocket for UdpSocket {
    type EventLoop = Core;

    type SendTo = SendTo;
    type RecvFrom = RecvFrom;

    fn from_socket<R>(socket: StdUdpSocket, handle: &R) -> Result<Self> where
        R: AsRegistrar<Core>,
    {
        trait AssertSend: Send {}
        impl AssertSend for TcpListener {}

        let handle = handle.as_registrar();
        let socket = PollEvented::new(MioUdpSocket::from_socket(socket)?, handle)?;
        Ok(UdpSocket {
            inner: Arc::new(socket),
        })
    }

    fn local_addr(&self) -> Result<SocketAddr> { self.inner.get_ref().local_addr() }

    fn connect(&self, addr: &SocketAddr) -> Result<()> {
        self.inner.get_ref().connect(addr.clone())
    }

    fn send_to(self, buffer: Bytes, addr: &SocketAddr) -> SendTo {
        SendTo {
            state: SendToState::Pending(self, buffer, addr.clone()),
        }
    }

    fn recv_from(self, buffer: BytesMut) -> RecvFrom {
        RecvFrom {
            state: RecvFromState::Pending(self, buffer),
        }
    }
}

pub struct Connect {
    state: ConnectState,
}

enum ConnectState {
    None,
    Pending(TcpStream),
    Error(Error),
}

pub struct Accept {
    state: AcceptState
}

enum AcceptState {
    None,
    Pending(TcpListener),
    WaitingForLoop(futures::sync::oneshot::Receiver<Result<TcpStream>>, SocketAddr),
}

impl Future for Connect {
    type Item = TcpStream;
    type Error = Error;

    fn poll(&mut self) -> Poll<TcpStream, Error> {
        match mem::replace(&mut self.state, ConnectState::None) {
            ConnectState::Pending(stream) => {
                match stream.inner.poll_write() {
                    Async::Ready(()) => Ok(Async::Ready(stream)),
                    Async::NotReady => {
                        self.state = ConnectState::Pending(stream);
                        Ok(Async::NotReady)
                    },
                }
            },
            ConnectState::Error(err) => Err(err),
            ConnectState::None => panic!("future has already completed"),
        }
    }
}

impl Future for Accept {
    type Item = (TcpStream, SocketAddr);
    type Error = Error;

    fn poll(&mut self) -> Poll<(TcpStream, SocketAddr), Error> {
        match mem::replace(&mut self.state, AcceptState::None) {
            AcceptState::Pending(listener) => {
                match listener.inner.poll_read() {
                    Async::Ready(()) => {},
                    Async::NotReady => {
                        self.state = AcceptState::Pending(listener);
                        return Ok(Async::NotReady);
                    }
                }
                match listener.inner.get_ref().accept() {
                    Ok((stream, remote_addr)) => {
                        // If we are on the loop's thread we can finish immediately
                        if let Some(handle) = listener.inner.remote().handle() {
                            let stream = TcpStream {
                                inner: Arc::new(PollEvented::new(stream, &handle)?),
                            };
                            return Ok(Async::Ready((stream, remote_addr)));
                        }
                        // Otherwise we need to send the socket to the loop for registration
                        let (tx, rx) = futures::sync::oneshot::channel();
                        listener.inner.remote().spawn_fn(|handle| {
                            let result = PollEvented::new(stream, handle)
                                .map(move |stream| TcpStream { inner: Arc::new(stream) });
                            let _ = tx.send(result);
                            Ok(())
                        });

                        self.state = AcceptState::WaitingForLoop(rx, remote_addr);
                        // We need to poll to subscribe to the oneshot here
                        self.poll()
                    },
                    Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {
                        listener.inner.need_read();
                        self.state = AcceptState::Pending(listener);
                        return Ok(Async::NotReady);
                    },
                    Err(err) => return Err(err),
                }
            },
            AcceptState::WaitingForLoop(mut rx, remote_addr) => {
                match rx.poll() {
                    Ok(Async::Ready(stream)) => Ok(Async::Ready((stream?, remote_addr))),
                    Ok(Async::NotReady) => {
                        self.state = AcceptState::WaitingForLoop(rx, remote_addr);
                        Ok(Async::NotReady)
                    },
                    Err(_) => {
                        // Channel was cancelled, which indicates the loop is dead
                        return Err(io::Error::new(io::ErrorKind::Interrupted, "the loop processing this socket accept has died prematurely"));
                    }
                }
            },
            AcceptState::None => panic!("future has already completed"),
        }
    }
}

#[must_use = "futures do nothing unless polled"]
pub struct TcpWrite {
    state: TcpWriteState,
}

enum TcpWriteState {
    None,
    Pending(TcpStream, Bytes),
}


impl Future for TcpWrite {
    type Item = (TcpStream, Bytes, usize);
    type Error = Error;

    fn poll(&mut self) -> Poll<(TcpStream, Bytes, usize), Error> {
        let written = match self.state {
            TcpWriteState::Pending(ref stream, ref buffer) => {
                if let Async::NotReady = stream.inner.poll_write() {
                    return Ok(Async::NotReady);
                }
                match stream.inner.get_ref().write(buffer.as_ref()) {
                    Ok(bytes) => Ok(bytes),
                    Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {
                        stream.inner.need_write();
                        return Ok(Async::NotReady);
                    },
                    Err(err) => Err(err),
                }
            },
            TcpWriteState::None => panic!("future has already completed"),
        };

        match written {
            Ok(written) => {
                if let TcpWriteState::Pending(stream, buffer) = mem::replace(&mut self.state, TcpWriteState::None) {
                    Ok(Async::Ready((stream, buffer, written)))
                } else {
                    unreachable!()
                }
            },
            Err(err) => {
                self.state = TcpWriteState::None;
                Err(err)
            },
        }
    }
}

#[must_use = "futures do nothing unless polled"]
pub struct TcpRead {
    state: TcpReadState,
}

enum TcpReadState {
    None,
    Pending(TcpStream, BytesMut),
}

impl Future for TcpRead {
    type Item = (TcpStream, BytesMut, usize);
    type Error = Error;

    fn poll(&mut self) -> Poll<(TcpStream, BytesMut, usize), Error> {
        let read = match self.state {
            TcpReadState::Pending(ref stream, ref mut buffer) => {
                if let Async::NotReady = stream.inner.poll_read() {
                    return Ok(Async::NotReady);
                }
                match stream.inner.get_ref().read(buffer.as_mut()) {
                    Ok(bytes) => Ok(bytes),
                    Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {
                        stream.inner.need_read();
                        return Ok(Async::NotReady);
                    },
                    Err(err) => Err(err),
                }
            },
            TcpReadState::None => panic!("future has already completed"),
        };

        match read {
            Ok(read) => {
                if let TcpReadState::Pending(stream, buffer) = mem::replace(&mut self.state, TcpReadState::None) {
                    Ok(Async::Ready((stream, buffer, read)))
                } else {
                    unreachable!()
                }
            },
            Err(err) => {
                self.state = TcpReadState::None;
                Err(err)
            },
        }
    }
}

#[must_use = "futures do nothing unless polled"]
pub struct RecvFrom {
    state: RecvFromState,
}

enum RecvFromState {
    None,
    Pending(UdpSocket, BytesMut),
}

impl Future for RecvFrom {
    type Item = (UdpSocket, BytesMut, usize, SocketAddr);
    type Error = Error;
    
    fn poll(&mut self) -> Poll<(UdpSocket, BytesMut, usize, SocketAddr), Error> {
        let result = match self.state {
            RecvFromState::Pending(ref socket, ref mut buffer) => {
                if let Async::NotReady = socket.inner.poll_read() {
                    return Ok(Async::NotReady);
                }
                match socket.inner.get_ref().recv_from(buffer) {
                    Ok((read, remote_addr)) => Ok((read, remote_addr)),
                    Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {
                        socket.inner.need_read();
                        return Ok(Async::NotReady);
                    },
                    Err(err) => Err(err),
                }
            },
            RecvFromState::None => panic!("future has already completed"),
        };

        match result {
            Ok((read, addr)) => {
                if let RecvFromState::Pending(socket, buffer) = mem::replace(&mut self.state, RecvFromState::None) {
                    Ok(Async::Ready((socket, buffer, read, addr)))
                } else {
                    unreachable!()
                }
            },
            Err(err) => {
                self.state = RecvFromState::None;
                Err(err)
            },
        }
    }
}

#[must_use = "futures do nothing unless polled"]
pub struct SendTo {
    state: SendToState,
}

enum SendToState {
    None,
    Pending(UdpSocket, Bytes, SocketAddr),
}

impl Future for SendTo {
    type Item = (UdpSocket, Bytes);
    type Error = Error;
    
    fn poll(&mut self) -> Poll<(UdpSocket, Bytes), Error> {
        let result = match self.state {
            SendToState::Pending(ref socket, ref buffer, ref addr) => {
                if let Async::NotReady = socket.inner.poll_write() {
                    return Ok(Async::NotReady);
                }
                match socket.inner.get_ref().send_to(&buffer, addr) {
                    Ok(bytes) => if bytes == buffer.len() {
                        Ok(())
                    } else {
                        Err(io::ErrorKind::WriteZero.into())
                    },
                    Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {
                        socket.inner.need_write();
                        return Ok(Async::NotReady);
                    },
                    Err(err) => Err(err),
                }
            },
            SendToState::None => panic!("future has already completed"),
        };

        match result {
            Ok(()) => {
                if let SendToState::Pending(socket, buffer, _) = mem::replace(&mut self.state, SendToState::None) {
                    Ok(Async::Ready((socket, buffer)))
                } else {
                    unreachable!()
                }
            },
            Err(err) => {
                self.state = SendToState::None;
                Err(err)
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    make_net_tests!(Core::new().unwrap());
}
