use ::evloop::{EventLoop, LocalHandle, RemoteHandle, Registrar, AsRegistrar};
use ::net::{NetEventLoop, SendTo, RecvFrom};
use ::io::*;

use std::{mem, io};
use std::sync::Arc;
use std::time::Duration;
use std::io::{Error, Result, Read as IoRead, Write as IoWrite};
use std::net::{SocketAddr, TcpStream as StdTcpStream, TcpListener as StdTcpListener, UdpSocket as StdUdpSocket};

use futures;
use futures::prelude::*;
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
    type FutureImpl = TcpRead;

    fn read<B: LockedBufferMut>(self, buffer: B) -> Read<B, TcpRead> {
        Read {
            buffer: Some(buffer),
            future: TcpRead {
                state: TcpReadState::Pending(self.clone())
            }
        }
    }
}

impl AsyncWrite for TcpStream {
    type FutureImpl = TcpWrite;

    fn write<B: LockedBuffer>(self, buffer: B) -> Write<B, TcpWrite> {
        Write {
            buffer: Some(buffer),
            future: TcpWrite {
                state: TcpWriteState::Pending(self.clone())
            }
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

    type SendToFutureImpl = SendToImpl;
    type RecvFromFutureImpl = RecvFromImpl;

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

    fn send_to<B: LockedBuffer>(self, buffer: B, addr: &SocketAddr) -> SendTo<B, SendToImpl> {
        SendTo {
            buffer: Some(buffer),
            future: SendToImpl {
                state: SendToState::Pending(self, addr.clone()),
            },
        }
    }

    fn recv_from<B: LockedBufferMut>(self, buffer: B) -> RecvFrom<B, RecvFromImpl> {
        RecvFrom {
            buffer: Some(buffer),
            future: RecvFromImpl {
                state: RecvFromState::Pending(self),
            },
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
#[doc(hidden)]
pub struct TcpWrite {
    state: TcpWriteState,
}

enum TcpWriteState {
    None,
    Pending(TcpStream),
}

impl IoFutureImpl for TcpWrite {
}

impl IoWriteFutureImpl for TcpWrite {
    type Item = (TcpStream, usize);

    fn poll<B: LockedBuffer>(&mut self, buffer: &B) -> Poll<(TcpStream, usize), Error> {
        let bytes = match self.state {
            TcpWriteState::Pending(ref stream) => {
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

        match bytes {
            Ok(bytes) => {
                if let TcpWriteState::Pending(stream) = mem::replace(&mut self.state, TcpWriteState::None) {
                    Ok(Async::Ready((stream, bytes)))
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
#[doc(hidden)]
pub struct TcpRead {
    state: TcpReadState,
}

enum TcpReadState {
    None,
    Pending(TcpStream),
}

impl IoFutureImpl for TcpRead {
}

impl IoReadFutureImpl for TcpRead {
    type Item = (TcpStream, usize);

    fn poll<B: LockedBufferMut>(&mut self, buffer: &mut B) -> Poll<(TcpStream, usize), Error> {
        let bytes = match self.state {
            TcpReadState::Pending(ref stream) => {
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

        match bytes {
            Ok(bytes) => {
                if let TcpReadState::Pending(stream) = mem::replace(&mut self.state, TcpReadState::None) {
                    Ok(Async::Ready((stream, bytes)))
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

#[doc(hidden)]
pub struct RecvFromImpl {
    state: RecvFromState,
}

enum RecvFromState {
    None,
    Pending(UdpSocket),
}

impl IoFutureImpl for RecvFromImpl {
}

impl IoReadFutureImpl for RecvFromImpl {
    type Item = (UdpSocket, usize, SocketAddr);
    
    fn poll<B: LockedBufferMut>(&mut self, buffer: &mut B) -> Poll<(UdpSocket, usize, SocketAddr), Error> {
        let result = match self.state {
            RecvFromState::Pending(ref socket) => {
                if let Async::NotReady = socket.inner.poll_read() {
                    return Ok(Async::NotReady);
                }
                match socket.inner.get_ref().recv_from(buffer.as_mut()) {
                    Ok((bytes, remote_addr)) => Ok((bytes, remote_addr)),
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
            Ok((bytes, addr)) => {
                if let RecvFromState::Pending(socket) = mem::replace(&mut self.state, RecvFromState::None) {
                    Ok(Async::Ready((socket, bytes, addr)))
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

#[doc(hidden)]
pub struct SendToImpl {
    state: SendToState,
}

enum SendToState {
    None,
    Pending(UdpSocket, SocketAddr),
}

impl IoFutureImpl for SendToImpl {
}

impl IoWriteFutureImpl for SendToImpl {
    type Item = UdpSocket;
    
    fn poll<B: LockedBuffer>(&mut self, buffer: &B) -> Poll<UdpSocket, Error> {
        let result = match self.state {
            SendToState::Pending(ref socket, ref addr) => {
                if let Async::NotReady = socket.inner.poll_write() {
                    return Ok(Async::NotReady);
                }
                match socket.inner.get_ref().send_to(buffer.as_ref(), addr) {
                    Ok(bytes) => if bytes == buffer.as_ref().len() {
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
                if let SendToState::Pending(socket, _) = mem::replace(&mut self.state, SendToState::None) {
                    Ok(Async::Ready(socket))
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
