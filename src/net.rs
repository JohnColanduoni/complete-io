use ::evloop::{EventLoop, ConcurrentEventLoop, AsRegistrar};
use ::io::*;

use std::{mem};
use std::io::{Error, Result};
use std::net::{SocketAddr, TcpListener as StdTcpListener, TcpStream as StdTcpStream, UdpSocket as StdUdpSocket};

use futures::prelude::*;

pub trait TcpListener: Sized + Clone {
    type EventLoop: EventLoop;
    type TcpStream: TcpStream<EventLoop = Self::EventLoop>;

    type Accept: Future<Item = (Self::TcpStream, SocketAddr), Error = Error>;

    fn bind<R>(addr: &SocketAddr, handle: &R) -> Result<Self> where
        R: AsRegistrar<Self::EventLoop>,
    {
        Self::from_listener(StdTcpListener::bind(addr)?, handle)
    }

    fn from_listener<R>(listener: StdTcpListener, handle: &R) -> Result<Self> where
        R: AsRegistrar<Self::EventLoop>;

    fn accept(&self) -> Self::Accept;

    fn local_addr(&self) -> Result<SocketAddr>;
}

pub trait TcpStream: Sized + AsyncRead + AsyncWrite + Clone {
    type EventLoop: EventLoop;

    type Connect: Future<Item = Self, Error = Error>;

    fn connect<R>(addr: &SocketAddr, handle: &R) -> Self::Connect where
        R: AsRegistrar<Self::EventLoop>;

    fn from_stream<R>(stream: StdTcpStream, handle: &R) -> Result<Self> where
        R: AsRegistrar<Self::EventLoop>;

    fn local_addr(&self) -> Result<SocketAddr>;
}

pub trait UdpSocket: Sized + Clone {
    type EventLoop: EventLoop;

    #[doc(hidden)]
    type SendToFutureImpl: IoWriteFutureImpl<Item = Self>;
    #[doc(hidden)]
    type RecvFromFutureImpl: IoReadFutureImpl<Item = (Self, usize, SocketAddr)>;
    
    fn bind<R>(addr: &SocketAddr, handle: &R) -> Result<Self> where
        R: AsRegistrar<Self::EventLoop>,
    {
        Self::from_socket(StdUdpSocket::bind(addr)?, handle)
    }

    fn from_socket<R>(socket: StdUdpSocket, handle: &R) -> Result<Self> where
        R: AsRegistrar<Self::EventLoop>;

    fn local_addr(&self) -> Result<SocketAddr>;

    fn connect(&self, addr: &SocketAddr) -> Result<()>;

    fn send_to<B: LockedBuffer>(self, buffer: B, addr: &SocketAddr) -> SendTo<B, Self::SendToFutureImpl>;

    fn recv_from<B: LockedBufferMut>(self, buffer: B) -> RecvFrom<B, Self::RecvFromFutureImpl>;
}

pub trait NetEventLoop: EventLoop {
    type TcpListener: TcpListener<EventLoop = Self, TcpStream = Self::TcpStream>;
    type TcpStream: TcpStream<EventLoop = Self>;
    type UdpSocket: UdpSocket<EventLoop = Self>;
}

pub trait ConcurrentNetEventLoop: ConcurrentEventLoop + NetEventLoop where
    Self::TcpListener: Send,
    Self::TcpStream: Send,
    Self::UdpSocket: Send,
    Self::Registrar: Send,
    Self::RemoteHandle: AsRegistrar<Self>,
{ }

#[must_use = "futures do nothing unless polled"]
pub struct RecvFrom<B, F: IoReadFutureImpl> {
    #[doc(hidden)]
    pub buffer: Option<B>,
    #[doc(hidden)]
    pub future: F,
}

#[must_use = "futures do nothing unless polled"]
pub struct SendTo<B, F: IoWriteFutureImpl> {
    #[doc(hidden)]
    pub buffer: Option<B>,
    #[doc(hidden)]
    pub future: F,
}

impl<B, F: IoReadFutureImpl> Drop for RecvFrom<B, F> {
    fn drop(&mut self) {
        if let Some(buffer) = self.buffer.take() {
            if let Err(err) = self.future.cancel() {
                warn!("leaking buffer because cancellation of IO operation failed: {:?}", err);
                mem::forget(buffer);
            }
        }
    }
}

impl<B, F: IoWriteFutureImpl> Drop for SendTo<B, F> {
    fn drop(&mut self) {
        if let Some(buffer) = self.buffer.take() {
            if let Err(err) = self.future.cancel() {
                warn!("leaking buffer because cancellation of IO operation failed: {:?}", err);
                mem::forget(buffer);
            }
        }
    }
}

impl<B, F, T1: Sized> Future for RecvFrom<B, F> where
    B: LockedBufferMut,
    F: IoReadFutureImpl<Item = (T1, usize, SocketAddr)>,
{
    type Item = (T1, B, usize, SocketAddr);
    type Error = Error;

    fn poll(&mut self) -> Poll<(T1, B, usize, SocketAddr), Error> {
        let (t1, t2, t3) = match self.buffer {
            Some(ref mut buffer) => {
                try_ready!(self.future.poll(buffer))
            },
            None => panic!("future already completed"),
        };

        Ok(Async::Ready((t1, self.buffer.take().unwrap(), t2, t3)))
    }
}

impl<B, F, T1: Sized> Future for SendTo<B, F> where
    B: LockedBuffer,
    F: IoWriteFutureImpl<Item = T1>,
{
    type Item = (T1, B);
    type Error = Error;

    fn poll(&mut self) -> Poll<(T1, B), Error> {
        let t1 = match self.buffer {
            Some(ref buffer) => {
                try_ready!(self.future.poll(buffer))
            },
            None => panic!("future already completed"),
        };

        Ok(Async::Ready((t1, self.buffer.take().unwrap())))
    }
}

#[cfg(test)]
macro_rules! make_net_tests {
    ($make_evloop:expr) => {
        #[allow(unused)]
        use ::evloop::{EventLoop};
        use ::net::{TcpListener as GenTcpListener, TcpStream as GenTcpStream, UdpSocket as GenUdpSocket};

        use std::net::{SocketAddr, SocketAddrV4, SocketAddrV6, Ipv4Addr};

        use futures::future;
        use net2::TcpBuilder;

        #[test]
        fn accept_connect() {
            let mut iocp = $make_evloop;
            let handle = iocp.handle();
            let listener = TcpListener::bind(&"0.0.0.0:0".parse().unwrap(), &handle).unwrap();
            let mut listener_addr = listener.local_addr().unwrap();
            listener_addr.set_ip("127.0.0.1".parse().unwrap());
            let ((_, remote_addr), client_side) = iocp.run(future::lazy(|| {
                let accepted = listener.accept();
                let connected = TcpStream::connect(&listener_addr, &handle);
                accepted.join(connected)
            })).unwrap();
            assert_eq!(SocketAddr::V4(SocketAddrV4::new("127.0.0.1".parse().unwrap(), client_side.local_addr().unwrap().port())), remote_addr);
        }

        #[test]
        fn accept_connect_ipv6() {
            let mut iocp = $make_evloop;
            let handle = iocp.handle();
            let listener = TcpBuilder::new_v6().unwrap()
                .only_v6(true).unwrap()
                .bind("[::]:0").unwrap()
                .listen(8).unwrap();
            let listener = TcpListener::from_listener(listener, &handle).unwrap();
            let mut listener_addr = listener.local_addr().unwrap();
            listener_addr.set_ip("::1".parse().unwrap());
            let ((_, remote_addr), client_side) = iocp.run(future::lazy(|| {
                let accepted = listener.accept();
                let connected = TcpStream::connect(&listener_addr, &handle);
                accepted.join(connected)
            })).unwrap();
            assert_eq!(SocketAddr::V6(SocketAddrV6::new("::1".parse().unwrap(), client_side.local_addr().unwrap().port(), 0, 0)), remote_addr);
        }

        #[test]
        fn accept_connect_dual_stack() {
            let mut iocp = $make_evloop;
            let handle = iocp.handle();
            let listener = TcpBuilder::new_v6().unwrap()
                .only_v6(false).unwrap()
                .bind("[::]:0").unwrap()
                .listen(8).unwrap();
            let listener = TcpListener::from_listener(listener, &handle).unwrap();
            let mut listener_addr = listener.local_addr().unwrap();
            listener_addr.set_ip("127.0.0.1".parse().unwrap());
            let ((_, remote_addr), client_side) = iocp.run(future::lazy(|| {
                let accepted = listener.accept();
                let connected = TcpStream::connect(&listener_addr, &handle);
                accepted.join(connected)
            })).unwrap();
            assert_eq!(SocketAddr::V6(SocketAddrV6::new(Ipv4Addr::new(127, 0, 0, 1).to_ipv6_mapped(), client_side.local_addr().unwrap().port(), 0, 0)), remote_addr);
        }

        #[test]
        fn tcp_echo() {
            let mut evloop = $make_evloop;
            let handle = evloop.handle();
            let listener = TcpListener::bind(&"0.0.0.0:0".parse().unwrap(), &handle).unwrap();
            let mut listener_addr = listener.local_addr().unwrap();
            listener_addr.set_ip("127.0.0.1".parse().unwrap());
            evloop.run(future::lazy(|| {
                let accepted = listener.accept();
                let connected = TcpStream::connect(&listener_addr, &handle);
                accepted.join(connected).and_then(|((server, _), client)| {
                    let message = "hello";
                    let client_fut = 
                        client
                            .write(message.to_string().into_bytes())
                            .and_then(|(client, buffer, _bytes)| client.read(buffer));
                    let server_fut =
                        server
                            .read(vec![0u8; message.len()])
                            .and_then(|(server, buffer, _bytes)| server.write(buffer));

                    client_fut.join(server_fut)
                })
            })).unwrap();
        }

        #[test]
        fn udp_echo() {
            let mut evloop = $make_evloop;
            let handle = evloop.handle();
            let server = UdpSocket::bind(&"0.0.0.0:0".parse().unwrap(), &handle).unwrap();
            let mut server_addr = server.local_addr().unwrap();
            server_addr.set_ip("127.0.0.1".parse().unwrap());
            let client = UdpSocket::bind(&"0.0.0.0:0".parse().unwrap(), &handle).unwrap();
            evloop.run(future::lazy(|| {
                let message = "hello";
                let client_fut = 
                    client
                        .send_to(message.to_string().into_bytes(), &server_addr)
                        .and_then(|(client, buffer)| client.recv_from(buffer));
                let server_fut =
                    server
                        .recv_from(vec![0u8; message.len()])
                        .and_then(|(server, buffer, _bytes, remote_addr)| server.send_to(buffer, &remote_addr));

                client_fut.join(server_fut)
            })).unwrap();
        }

    }
} 