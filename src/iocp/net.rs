use ::evloop::{AsRegistrar};
use ::iocp::{CompletionPort, RemoteHandle, OverlappedTask};
use ::net::{NetEventLoop};
use ::io::*;

use std::{mem, io};
use std::sync::Arc;
use std::io::{Error, Result};
use std::net::{SocketAddr, IpAddr, Ipv4Addr, Ipv6Addr, TcpListener as StdTcpListener, TcpStream as StdTcpStream, UdpSocket as StdUdpSocket};
use std::os::windows::prelude::*;

use futures::prelude::*;
use bytes::{Bytes, BytesMut};
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

impl ::net::TcpListener for TcpListener {
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

impl ::net::TcpStream for TcpStream {
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
    type Read = TcpRead;

    fn read(self, mut buffer: BytesMut) -> TcpRead {
        ensure_bytesmut_safe(&mut buffer);
        TcpRead { state: TcpReadState::Initial(self.clone(), buffer) }
    }
}

impl AsyncWrite for TcpStream {
    type Write = TcpWrite;

    fn write(self, mut buffer: Bytes) -> TcpWrite {
        ensure_bytes_safe(&mut buffer);
        TcpWrite { state: TcpWriteState::Initial(self.clone(), buffer) }
    }
}

impl AsRawSocket for TcpStream {
    fn as_raw_socket(&self) -> SOCKET { self.inner.std.as_raw_socket() }
}

impl ::net::UdpSocket for UdpSocket {
    type EventLoop = CompletionPort;

    type RecvFrom = RecvFrom;
    type SendTo = SendTo;

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

    fn send_to(self, mut buffer: Bytes, addr: &SocketAddr) -> SendTo {
        ensure_bytes_safe(&mut buffer);
        SendTo {
            state: SendToState::Initial(self, buffer, addr.clone()),
        }
    }

    fn recv_from(self, mut buffer: BytesMut) -> RecvFrom {
        ensure_bytesmut_safe(&mut buffer);
        RecvFrom {
            state: RecvFromState::Initial(self, buffer),
        }
    }
}

impl AsRawSocket for UdpSocket {
    fn as_raw_socket(&self) -> SOCKET { self.inner.std.as_raw_socket() }
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
pub struct TcpWrite {
    state: TcpWriteState,
}

enum TcpWriteState {
    None,
    Initial(TcpStream, Bytes),
    Pending(TcpStream, Bytes, OverlappedTask),
}

impl Drop for TcpWrite {
    fn drop(&mut self) {
        match mem::replace(&mut self.state, TcpWriteState::None) {
            TcpWriteState::Pending(socket, buf, overlapped) => {
                if let Err((_, err)) = overlapped.cancel_socket(&socket) {
                    error!("cancelation of overlapped operation failed, leaking buffers for safety: {:?}", err);
                    mem::forget(buf);
                }
            },
            _ => {},
        }
    }
}

impl Future for TcpWrite {
    type Item = (TcpStream, Bytes, usize);
    type Error = Error;

    fn poll(&mut self) -> Poll<(TcpStream, Bytes, usize), Error> {
        match mem::replace(&mut self.state, TcpWriteState::None) {
            TcpWriteState::Initial(stream, buffer) => {
                let task = OverlappedTask::new(&stream.inner.evloop);
                match unsafe { task.clone().for_operation(|overlapped| stream.inner.std.write_overlapped(buffer.as_ref(), overlapped)) }? {
                    Some(bytes) => Ok(Async::Ready((stream, buffer, bytes))),
                    None => {
                        self.state = TcpWriteState::Pending(stream, buffer, task);
                        Ok(Async::NotReady)
                    },
                }
            },
            TcpWriteState::Pending(stream, buffer, task) => {
                task.register();
                match task.poll_socket(&stream.inner.std)? {
                    Async::Ready((bytes, _)) => Ok(Async::Ready((stream, buffer, bytes))),
                    Async::NotReady => {
                        self.state = TcpWriteState::Pending(stream, buffer, task);
                        Ok(Async::NotReady)
                    },
                }
            },
            TcpWriteState::None => panic!("future has already completed"),
        }
    }
}

#[must_use = "futures do nothing unless polled"]
pub struct TcpRead {
    state: TcpReadState,
}

enum TcpReadState {
    None,
    Initial(TcpStream, BytesMut),
    Pending(TcpStream, BytesMut, OverlappedTask),
}

impl Drop for TcpRead {
    fn drop(&mut self) {
        match mem::replace(&mut self.state, TcpReadState::None) {
            TcpReadState::Pending(socket, buf, overlapped) => {
                if let Err((_, err)) = overlapped.cancel_socket(&socket) {
                    error!("cancelation of overlapped operation failed, leaking buffers for safety: {:?}", err);
                    mem::forget(buf);
                }
            },
            _ => {},
        }
    }
}

impl Future for TcpRead {
    type Item = (TcpStream, BytesMut, usize);
    type Error = Error;
    
    fn poll(&mut self) -> Poll<(TcpStream, BytesMut, usize), Error> {
        match mem::replace(&mut self.state, TcpReadState::None) {
            TcpReadState::Initial(stream, mut buffer) => {
                let task = OverlappedTask::new(&stream.inner.evloop);
                match unsafe { task.clone().for_operation(|overlapped| stream.inner.std.read_overlapped(&mut buffer, overlapped)) }? {
                    Some(bytes) => Ok(Async::Ready((stream, buffer, bytes))),
                    None => {
                        self.state = TcpReadState::Pending(stream, buffer, task);
                        Ok(Async::NotReady)
                    },
                }
            },
            TcpReadState::Pending(stream, buffer, task) => {
                task.register();
                match task.poll_socket(&stream.inner.std)? {
                    Async::Ready((bytes, _)) => Ok(Async::Ready((stream, buffer, bytes))),
                    Async::NotReady => {
                        self.state = TcpReadState::Pending(stream, buffer, task);
                        Ok(Async::NotReady)
                    },
                }
            },
            TcpReadState::None => panic!("future has already completed"),
        }
    }
}


#[must_use = "futures do nothing unless polled"]
pub struct RecvFrom {
    state: RecvFromState,
}

enum RecvFromState {
    None,
    Initial(UdpSocket, BytesMut),
    Pending(UdpSocket, BytesMut, Box<SocketAddrBuf>, OverlappedTask),
}

impl Drop for RecvFrom {
    fn drop(&mut self) {
        match mem::replace(&mut self.state, RecvFromState::None) {
            RecvFromState::Pending(socket, buf, addr_buf, overlapped) => {
                if let Err((_, err)) = overlapped.cancel_socket(&socket) {
                    error!("cancelation of overlapped operation failed, leaking buffers for safety: {:?}", err);
                    mem::forget(buf);
                    mem::forget(addr_buf);
                }
            },
            _ => {},
        }
    }
}

impl Future for RecvFrom {
    type Item = (UdpSocket, BytesMut, usize, SocketAddr);
    type Error = Error;
    
    fn poll(&mut self) -> Poll<(UdpSocket, BytesMut, usize, SocketAddr), Error> {
        match mem::replace(&mut self.state, RecvFromState::None) {
            RecvFromState::Initial(socket, mut buffer) => {
                let task = OverlappedTask::new(&socket.inner.evloop); // TODO: pool tasks
                let mut addr_buf = Box::new(SocketAddrBuf::new()); // TODO: pool buffers
                match unsafe { task.clone().for_operation(|overlapped| socket.inner.std.recv_from_overlapped(&mut buffer, &mut *addr_buf, overlapped)) }? {
                    Some(read) => Ok(Async::Ready((socket, buffer, read, addr_buf.to_socket_addr().unwrap()))),
                    None => {
                        self.state = RecvFromState::Pending(socket, buffer, addr_buf, task);
                        Ok(Async::NotReady)
                    },
                }
            },
            RecvFromState::Pending(socket, buffer, addr_buf, task) => {
                task.register();
                match task.poll_socket(&socket.inner.std)? {
                    Async::Ready((read, _)) => Ok(Async::Ready((socket, buffer, read, addr_buf.to_socket_addr().unwrap()))),
                    Async::NotReady => {
                        self.state = RecvFromState::Pending(socket, buffer, addr_buf, task);
                        Ok(Async::NotReady)
                    },
                }
            },
            RecvFromState::None => panic!("future has already completed"),
        }
    }
}

#[must_use = "futures do nothing unless polled"]
pub struct SendTo {
    state: SendToState,
}

enum SendToState {
    None,
    Initial(UdpSocket, Bytes, SocketAddr),
    Pending(UdpSocket, Bytes, OverlappedTask),
}

impl Drop for SendTo {
    fn drop(&mut self) {
        match mem::replace(&mut self.state, SendToState::None) {
            SendToState::Pending(socket, buf, overlapped) => {
                if let Err((_, err)) = overlapped.cancel_socket(&socket) {
                    error!("cancelation of overlapped operation failed, leaking buffers for safety: {:?}", err);
                    mem::forget(buf);
                }
            },
            _ => {},
        }
    }
}

impl Future for SendTo {
    type Item = (UdpSocket, Bytes);
    type Error = Error;
    
    fn poll(&mut self) -> Poll<(UdpSocket, Bytes), Error> {
        match mem::replace(&mut self.state, SendToState::None) {
            SendToState::Initial(socket, buffer, addr) => {
                let task = OverlappedTask::new(&socket.inner.evloop); // TODO: pool tasks
                match unsafe { task.clone().for_operation(|overlapped| socket.inner.std.send_to_overlapped(buffer.as_ref(), &addr, overlapped)) }? {
                    Some(bytes) => {
                        if bytes == buffer.as_ref().len() {
                            Ok(Async::Ready((socket, buffer)))
                        } else {
                            Err(io::ErrorKind::WriteZero.into())
                        }
                    },
                    None => {
                        self.state = SendToState::Pending(socket, buffer, task);
                        Ok(Async::NotReady)
                    },
                }
            },
            SendToState::Pending(socket, buffer, task) => {
                task.register();
                match task.poll_socket(&socket.inner.std)? {
                    Async::Ready((bytes, _)) => {
                        if bytes == buffer.as_ref().len() {
                            Ok(Async::Ready((socket, buffer)))
                        } else {
                            Err(io::ErrorKind::WriteZero.into())
                        }
                    },
                    Async::NotReady => {
                        self.state = SendToState::Pending(socket, buffer, task);
                        Ok(Async::NotReady)
                    },
                }
            },
            SendToState::None => panic!("future has already completed"),
        }
    }
}

// Bytes can store data inline. Ensure this is not the case here
fn ensure_bytes_safe(bytes: &mut Bytes) {
    let data_ptr = bytes.as_ptr() as usize;
    let struct_ptr = bytes as *const Bytes;
    let struct_end = unsafe { struct_ptr.offset(1) };
    if data_ptr >= struct_ptr as usize && data_ptr < struct_end as usize {
        *bytes = Bytes::from(bytes.to_vec());
    }
}
// BytesMut can store data inline. Ensure this is not the case here
fn ensure_bytesmut_safe(bytes: &mut BytesMut) {
    let data_ptr = bytes.as_ptr() as usize;
    let struct_ptr = bytes as *const BytesMut;
    let struct_end = unsafe { struct_ptr.offset(1) };
    if data_ptr >= struct_ptr as usize && data_ptr < struct_end as usize {
        *bytes = BytesMut::from(bytes.to_vec());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::iocp::CompletionPort;

    make_net_tests!(CompletionPort::new(1).unwrap());
}