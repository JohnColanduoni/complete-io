use ::evloop::{EventLoop, ConcurrentEventLoop, AsRegistrar};

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

pub trait TcpStream: Sized + Clone {
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
    
    fn bind<R>(addr: &SocketAddr, handle: &R) -> Result<Self> where
        R: AsRegistrar<Self::EventLoop>,
    {
        Self::from_socket(StdUdpSocket::bind(addr)?, handle)
    }

    fn from_socket<R>(socket: StdUdpSocket, handle: &R) -> Result<Self> where
        R: AsRegistrar<Self::EventLoop>;

    fn local_addr(&self) -> Result<SocketAddr>;

    fn connect(&self, addr: &SocketAddr) -> Result<()>;
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

#[cfg(test)]
macro_rules! make_net_tests {
    ($make_evloop:expr) => {
        use ::evloop::{EventLoop};

        use std::net::{SocketAddr, SocketAddrV4, SocketAddrV6, Ipv4Addr, Ipv6Addr};

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
    }
} 