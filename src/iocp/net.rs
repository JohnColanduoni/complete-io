use ::evloop::{AsRegistrar};
use ::iocp::{CompletionPort, RemoteHandle, OverlappedTask};
use ::net::{TcpListener as GenTcpListener, TcpStream as GenTcpStream, UdpSocket as GenUdpSocket, NetEventLoop, SendTo, RecvFrom};
use ::genio::*;

use std::{mem, io};
use std::sync::Arc;
use std::io::{Error, Result};
use std::net::{SocketAddr, IpAddr, Ipv4Addr, Ipv6Addr, TcpListener as StdTcpListener, TcpStream as StdTcpStream, UdpSocket as StdUdpSocket};
use std::os::windows::prelude::*;

use futures::prelude::*;
use winapi::*;
use ws2_32::*;
use net2::TcpBuilder;
use miow::net::*;

#[derive(Clone)]
pub struct TcpListener {
    inner: Arc<_TcpListener>,
}

struct _TcpListener {
    std: StdTcpListener,
    evloop: RemoteHandle,
    ip_version: IpVersion,
}

enum IpVersion {
    V4Only,
    V6Only,
    DualStack,
}

#[derive(Clone)]
pub struct TcpStream {
    inner: Arc<_TcpStream>,
}

struct _TcpStream {
    std: StdTcpStream,
    evloop: RemoteHandle,
}

#[derive(Clone)]
pub struct UdpSocket {
    inner: Arc<_UdpSocket>,
}

struct _UdpSocket {
    std: StdUdpSocket,
    evloop: RemoteHandle,
}

impl GenTcpListener for TcpListener {
    type EventLoop = CompletionPort;
    type TcpStream = TcpStream;
    type Accept = Accept;

    fn from_listener<R>(listener: StdTcpListener, handle: &R) -> Result<Self> where
        R: AsRegistrar<CompletionPort>,
    {
        let handle = handle.as_registrar();
        handle.add_socket(&listener)?;

        let ip_version = if listener.local_addr()?.is_ipv4() {
            IpVersion::V4Only
        } else {
            unsafe {
                let mut value: DWORD = 0;
                let mut value_len: ::std::os::raw::c_int = mem::size_of_val(&value) as _;
                if getsockopt(
                    listener.as_raw_socket(),
                    IPPROTO_IPV6.0 as i32,
                    IPV6_V6ONLY,
                    &mut value as *mut DWORD as _,
                    &mut value_len,
                ) != 0 {
                    return Err(Error::from_raw_os_error(WSAGetLastError()));
                }

                if value == 0 {
                    IpVersion::DualStack
                } else {
                    IpVersion::V6Only
                }
            }
        };

        Ok(TcpListener {
            inner: Arc::new(_TcpListener {
                std: listener,
                evloop: handle.clone(),
                ip_version,
            }),
        })
    }

    fn accept(&self) -> Accept {
        Accept {
            state: AcceptState::Initial(self.inner.clone()),
        }
    }

    fn local_addr(&self) -> Result<SocketAddr> {
        self.inner.std.local_addr()
    }
}

impl AsRawSocket for TcpListener {
    fn as_raw_socket(&self) -> RawSocket {
        self.inner.std.as_raw_socket()
    }
}

impl TcpStream {
    fn from_accepted_stream(stream: StdTcpStream, listener: &_TcpListener) -> Result<Self> {
        listener.std.accept_complete(&stream)?;
        listener.evloop.add_socket(&stream)?;

        Ok(TcpStream {
            inner: Arc::new(_TcpStream {
                std: stream,
                evloop: listener.evloop.clone(),
            })
        })
    }

    fn from_connected_stream(stream: Arc<_TcpStream>) -> Result<Self> {
        stream.std.connect_complete()?;
        Ok(TcpStream { inner: stream })
    }
}

impl GenTcpStream for TcpStream {
    type EventLoop = CompletionPort;

    type Connect = Connect;

    fn connect<R>(addr: &SocketAddr, handle: &R) -> Connect where
        R: AsRegistrar<CompletionPort>,
    {
        let handle = handle.as_registrar();
        let state = (|| {
            // Windows requires the socket be bound prior to connection
            let std_stream = if addr.is_ipv4() {
                TcpBuilder::new_v4()?.bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 0))?.to_tcp_stream()?
            } else {
                TcpBuilder::new_v6()?.bind(SocketAddr::new(IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0)), 0))?.to_tcp_stream()?
            };
            handle.add_socket(&std_stream)?;

            Ok(ConnectState::Initial(Arc::new(_TcpStream {
                std: std_stream,
                evloop: handle.clone(),
            }), addr.clone()))
        })().unwrap_or_else(|err| ConnectState::Error(err));

        Connect { state }
    }

    fn from_stream<R>(stream: StdTcpStream, handle: &R) -> Result<Self> where
        R: AsRegistrar<CompletionPort>,
    {
        let handle = handle.as_registrar();
        handle.add_socket(&stream)?;

        Ok(TcpStream {
            inner: Arc::new(_TcpStream {
                std: stream,
                evloop: handle.clone(),
            }),
        })
    }

    fn local_addr(&self) -> Result<SocketAddr> {
        self.inner.std.local_addr()
    }
}

impl AsyncRead for TcpStream {
    type FutureImpl = TcpRead;

    fn read<B: LockedBufferMut>(self, buffer: B) -> Read<B, TcpRead> {
        Read {
            buffer: Some(buffer),
            future: TcpRead { state: TcpReadState::Initial(self.clone()) }
        }
    }
}

impl AsyncWrite for TcpStream {
    type FutureImpl = TcpWrite;

    fn write<B: LockedBuffer>(self, buffer: B) -> Write<B, TcpWrite> {
        Write {
            buffer: Some(buffer),
            future: TcpWrite { state: TcpWriteState::Initial(self.clone()) }
        }
    }
}

impl GenUdpSocket for UdpSocket {
    type EventLoop = CompletionPort;

    type RecvFromFutureImpl = RecvFromImpl;
    type SendToFutureImpl = SendToImpl;

    fn from_socket<R>(socket: StdUdpSocket, handle: &R) -> Result<Self> where
        R: AsRegistrar<Self::EventLoop>,
    {
        let handle = handle.as_registrar();
        handle.add_socket(&socket)?;

        Ok(UdpSocket {
            inner: Arc::new(_UdpSocket {
                std: socket,
                evloop: handle.clone(),
            })
        })
    }

    fn local_addr(&self) -> Result<SocketAddr> {
        self.inner.std.local_addr()
    }

    fn connect(&self, addr: &SocketAddr) -> Result<()> {
        self.inner.std.connect(addr)
    }

    fn send_to<B: LockedBuffer>(self, buffer: B, addr: &SocketAddr) -> SendTo<B, SendToImpl> {
        SendTo {
            buffer: Some(buffer),
            future: SendToImpl {
                state: SendToState::Initial(self, addr.clone()),
            },
        }
    }

    fn recv_from<B: LockedBufferMut>(self, buffer: B) -> RecvFrom<B, RecvFromImpl> {
        RecvFrom {
            buffer: Some(buffer),
            future: RecvFromImpl {
                state: RecvFromState::Initial(self),
            },
        }
    }
}

impl NetEventLoop for CompletionPort {
    type TcpStream = TcpStream;
    type TcpListener = TcpListener;
    type UdpSocket = UdpSocket;
}

#[must_use = "futures do nothing unless polled"]
pub struct Accept {
    state: AcceptState,
}

enum AcceptState {
    None,
    Initial(Arc<_TcpListener>),
    PendingSingleStack(Arc<_TcpListener>, StdTcpStream, Box<AcceptAddrsBuf>, OverlappedTask),
}

impl Future for Accept {
    type Item = (TcpStream, SocketAddr);
    type Error = Error;

    fn poll(&mut self) -> Poll<(TcpStream, SocketAddr), Error> {
        match mem::replace(&mut self.state, AcceptState::None) {
            AcceptState::None => panic!("future already completed"),
            AcceptState::Initial(listener) => {
                match listener.ip_version {
                    IpVersion::V4Only => {
                        match Self::initiate_v4(listener)? {
                            AcceptInitiateResult::Completed(stream, remote_addr) => 
                                Ok(Async::Ready((stream, remote_addr))),
                            AcceptInitiateResult::Pending(state) => {
                                self.state = state;
                                Ok(Async::NotReady)
                            },
                        }
                    },
                    IpVersion::V6Only | IpVersion::DualStack => {
                        match Self::initiate_v6(listener)? {
                            AcceptInitiateResult::Completed(stream, remote_addr) => 
                                Ok(Async::Ready((stream, remote_addr))),
                            AcceptInitiateResult::Pending(state) => {
                                self.state = state;
                                Ok(Async::NotReady)
                            },
                        }
                    },
                }
            },
            AcceptState::PendingSingleStack(listener, stream, addrs_buf, task) => {
                task.register();
                match task.poll_socket(&listener.std)? {
                    Async::NotReady => {
                        self.state = AcceptState::PendingSingleStack(listener, stream, addrs_buf, task);
                        return Ok(Async::NotReady);
                    },
                    Async::Ready(_) => {},
                }
                let stream = TcpStream::from_accepted_stream(stream, &listener)?;
                let remote_addr = addrs_buf.parse(&listener.std)?.remote().unwrap();
                Ok(Async::Ready((stream, remote_addr)))
            },
        }
    }
}

enum AcceptInitiateResult {
    Completed(TcpStream, SocketAddr),
    Pending(AcceptState),
}

impl Accept {
    fn initiate_v4(listener: Arc<_TcpListener>) -> Result<AcceptInitiateResult> {
        let acceptee = TcpBuilder::new_v4()?;
        let task = OverlappedTask::new(&listener.evloop);
        let mut addr_buffer = Box::new(AcceptAddrsBuf::new());
        let (acceptee, complete) = unsafe { task.clone().for_operation(|overlapped| listener.std.accept_overlapped(&acceptee, &mut *addr_buffer, overlapped))? };

        if complete {
            let stream = TcpStream::from_accepted_stream(acceptee, &listener)?;
            Ok(AcceptInitiateResult::Completed(stream, addr_buffer.parse(&listener.std)?.remote().unwrap()))
        } else {
            Ok(AcceptInitiateResult::Pending(AcceptState::PendingSingleStack(listener, acceptee, addr_buffer, task)))
        }
    }

    fn initiate_v6(listener: Arc<_TcpListener>) -> Result<AcceptInitiateResult> {
        let acceptee = TcpBuilder::new_v6()?;
        let task = OverlappedTask::new(&listener.evloop);
        let mut addr_buffer = Box::new(AcceptAddrsBuf::new());
        let (acceptee, complete) = unsafe { task.clone().for_operation(|overlapped| listener.std.accept_overlapped(&acceptee, &mut *addr_buffer, overlapped))? };

        if complete {
            let stream = TcpStream::from_accepted_stream(acceptee, &listener)?;
            Ok(AcceptInitiateResult::Completed(stream, addr_buffer.parse(&listener.std)?.remote().unwrap()))
        } else {
            Ok(AcceptInitiateResult::Pending(AcceptState::PendingSingleStack(listener, acceptee, addr_buffer, task)))
        }
    }
}

#[must_use = "futures do nothing unless polled"]
pub struct Connect {
    state: ConnectState,
}

enum ConnectState {
    None,
    Initial(Arc<_TcpStream>, SocketAddr),
    Pending(Arc<_TcpStream>, OverlappedTask),
    Error(Error),
}

impl Future for Connect {
    type Item = TcpStream;
    type Error = Error;

    fn poll(&mut self) -> Poll<TcpStream, Error> {
        match mem::replace(&mut self.state, ConnectState::None) {
            ConnectState::None => panic!("future already completed"),
            ConnectState::Initial(stream, addr) => {
                let task = OverlappedTask::new(&stream.evloop);
                let complete = unsafe { task.clone().for_operation(|overlapped| stream.std.connect_overlapped(&addr, &[], overlapped))? };
                if complete.is_some() {
                    Ok(Async::Ready(TcpStream::from_connected_stream(stream)?))
                } else {
                    self.state = ConnectState::Pending(stream, task);
                    Ok(Async::NotReady)
                }
            },
            ConnectState::Pending(stream, task) => {
                task.register();
                match task.poll_socket(&stream.std)? {
                    Async::NotReady => {
                        self.state = ConnectState::Pending(stream, task);
                        return Ok(Async::NotReady);
                    },
                    Async::Ready(_) => {},
                }
                Ok(Async::Ready(TcpStream::from_connected_stream(stream)?))
            },
            ConnectState::Error(error) => Err(error),
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
    Initial(TcpStream),
    Pending(TcpStream, OverlappedTask),
}

impl IoFutureImpl for TcpWrite {
    fn cancel(&mut self) -> Result<()> {
        unimplemented!()
    }
}

impl IoWriteFutureImpl for TcpWrite {
    type Item = (TcpStream, usize);

    fn poll<B: LockedBuffer>(&mut self, buffer: &B) -> Poll<(TcpStream, usize), Error> {
        match mem::replace(&mut self.state, TcpWriteState::None) {
            TcpWriteState::Initial(stream) => {
                let task = OverlappedTask::new(&stream.inner.evloop);
                match unsafe { task.clone().for_operation(|overlapped| stream.inner.std.write_overlapped(buffer.as_ref(), overlapped)) }? {
                    Some(bytes) => Ok(Async::Ready((stream, bytes))),
                    None => {
                        self.state = TcpWriteState::Pending(stream, task);
                        Ok(Async::NotReady)
                    },
                }
            },
            TcpWriteState::Pending(stream, task) => {
                task.register();
                match task.poll_socket(&stream.inner.std)? {
                    Async::Ready((bytes, _)) => Ok(Async::Ready((stream, bytes))),
                    Async::NotReady => {
                        self.state = TcpWriteState::Pending(stream, task);
                        Ok(Async::NotReady)
                    },
                }
            },
            TcpWriteState::None => panic!("future has already completed"),
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
    Initial(TcpStream),
    Pending(TcpStream, OverlappedTask),
}

impl IoFutureImpl for TcpRead {
    fn cancel(&mut self) -> Result<()> {
        unimplemented!()
    }
}

impl IoReadFutureImpl for TcpRead {
    type Item = (TcpStream, usize);
    
    fn poll<B: LockedBufferMut>(&mut self, buffer: &mut B) -> Poll<(TcpStream, usize), Error> {
        match mem::replace(&mut self.state, TcpReadState::None) {
            TcpReadState::Initial(stream) => {
                let task = OverlappedTask::new(&stream.inner.evloop);
                match unsafe { task.clone().for_operation(|overlapped| stream.inner.std.read_overlapped(buffer.as_mut(), overlapped)) }? {
                    Some(bytes) => Ok(Async::Ready((stream, bytes))),
                    None => {
                        self.state = TcpReadState::Pending(stream, task);
                        Ok(Async::NotReady)
                    },
                }
            },
            TcpReadState::Pending(stream, task) => {
                task.register();
                match task.poll_socket(&stream.inner.std)? {
                    Async::Ready((bytes, _)) => Ok(Async::Ready((stream, bytes))),
                    Async::NotReady => {
                        self.state = TcpReadState::Pending(stream, task);
                        Ok(Async::NotReady)
                    },
                }
            },
            TcpReadState::None => panic!("future has already completed"),
        }
    }
}


#[doc(hidden)]
pub struct RecvFromImpl {
    state: RecvFromState,
}

enum RecvFromState {
    None,
    Initial(UdpSocket),
    Pending(UdpSocket, Box<SocketAddrBuf>, OverlappedTask),
}

impl IoFutureImpl for RecvFromImpl {
    fn cancel(&mut self) -> Result<()> {
        unimplemented!()
    }
}

impl IoReadFutureImpl for RecvFromImpl {
    type Item = (UdpSocket, usize, SocketAddr);
    
    fn poll<B: LockedBufferMut>(&mut self, buffer: &mut B) -> Poll<(UdpSocket, usize, SocketAddr), Error> {
        match mem::replace(&mut self.state, RecvFromState::None) {
            RecvFromState::Initial(socket) => {
                let task = OverlappedTask::new(&socket.inner.evloop);
                let mut addr_buf = Box::new(SocketAddrBuf::new());
                match unsafe { task.clone().for_operation(|overlapped| socket.inner.std.recv_from_overlapped(buffer.as_mut(), &mut *addr_buf, overlapped)) }? {
                    Some(bytes) => Ok(Async::Ready((socket, bytes, addr_buf.to_socket_addr().unwrap()))),
                    None => {
                        self.state = RecvFromState::Pending(socket, addr_buf, task);
                        Ok(Async::NotReady)
                    },
                }
            },
            RecvFromState::Pending(socket, addr_buf, task) => {
                task.register();
                match task.poll_socket(&socket.inner.std)? {
                    Async::Ready((bytes, _)) => Ok(Async::Ready((socket, bytes, addr_buf.to_socket_addr().unwrap()))),
                    Async::NotReady => {
                        self.state = RecvFromState::Pending(socket, addr_buf, task);
                        Ok(Async::NotReady)
                    },
                }
            },
            RecvFromState::None => panic!("future has already completed"),
        }
    }
}

#[doc(hidden)]
pub struct SendToImpl {
    state: SendToState,
}

enum SendToState {
    None,
    Initial(UdpSocket, SocketAddr),
    Pending(UdpSocket, OverlappedTask),
}

impl IoFutureImpl for SendToImpl {
    fn cancel(&mut self) -> Result<()> {
        unimplemented!()
    }
}

impl IoWriteFutureImpl for SendToImpl {
    type Item = UdpSocket;
    
    fn poll<B: LockedBuffer>(&mut self, buffer: &B) -> Poll<UdpSocket, Error> {
        match mem::replace(&mut self.state, SendToState::None) {
            SendToState::Initial(socket, addr) => {
                let task = OverlappedTask::new(&socket.inner.evloop);
                match unsafe { task.clone().for_operation(|overlapped| socket.inner.std.send_to_overlapped(buffer.as_ref(), &addr, overlapped)) }? {
                    Some(bytes) => {
                        if bytes == buffer.as_ref().len() {
                            Ok(Async::Ready(socket))
                        } else {
                            Err(io::ErrorKind::WriteZero.into())
                        }
                    },
                    None => {
                        self.state = SendToState::Pending(socket, task);
                        Ok(Async::NotReady)
                    },
                }
            },
            SendToState::Pending(socket, task) => {
                task.register();
                match task.poll_socket(&socket.inner.std)? {
                    Async::Ready((bytes, _)) => {
                        if bytes == buffer.as_ref().len() {
                            Ok(Async::Ready(socket))
                        } else {
                            Err(io::ErrorKind::WriteZero.into())
                        }
                    },
                    Async::NotReady => {
                        self.state = SendToState::Pending(socket, task);
                        Ok(Async::NotReady)
                    },
                }
            },
            SendToState::None => panic!("future has already completed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::iocp::CompletionPort;

    make_net_tests!(CompletionPort::new(1).unwrap());
}