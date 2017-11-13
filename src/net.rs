use ::queue::{EventQueue};
use ::io::*;

use std::io::{Error, Result};
use std::net::{SocketAddr, TcpListener as StdTcpListener, TcpStream as StdTcpStream, UdpSocket as StdUdpSocket};

use futures::prelude::*;
use bytes::{Bytes, BytesMut};

pub trait TcpListener: Sized + Clone + Send {
    type EventQueue: EventQueue;
    type TcpStream: TcpStream<EventQueue = Self::EventQueue>;

    type Accept: Future<Item = (Self::TcpStream, SocketAddr), Error = Error>;

    fn bind(addr: &SocketAddr, handle: &<Self::EventQueue as EventQueue>::Handle) -> Result<Self> {
        Self::from_listener(StdTcpListener::bind(addr)?, handle)
    }

    fn from_listener(listener: StdTcpListener, handle: &<Self::EventQueue as EventQueue>::Handle) -> Result<Self>;

    fn accept(&self) -> Self::Accept;

    fn local_addr(&self) -> Result<SocketAddr>;
}

pub trait TcpStream: Sized + AsyncRead + AsyncWrite + Clone + Send {
    type EventQueue: EventQueue;

    type Connect: Future<Item = Self, Error = Error>;

    fn connect(addr: &SocketAddr, handle: &<Self::EventQueue as EventQueue>::Handle) -> Self::Connect;

    fn from_stream(stream: StdTcpStream, handle: &<Self::EventQueue as EventQueue>::Handle) -> Result<Self>;

    fn local_addr(&self) -> Result<SocketAddr>;
}

pub trait UdpSocket: Sized + Clone + Send {
    type EventQueue: EventQueue;

    type SendTo: Future<Item = (Self, Bytes), Error = Error> + Send + 'static;
    type RecvFrom: Future<Item = (Self, BytesMut, usize, SocketAddr), Error = Error> + Send + 'static;
    
    fn bind(addr: &SocketAddr, handle: &<Self::EventQueue as EventQueue>::Handle) -> Result<Self> {
        Self::from_socket(StdUdpSocket::bind(addr)?, handle)
    }

    fn from_socket(socket: StdUdpSocket, handle: &<Self::EventQueue as EventQueue>::Handle) -> Result<Self>;

    fn local_addr(&self) -> Result<SocketAddr>;

    fn connect(&self, addr: &SocketAddr) -> Result<()>;

    fn send_to(self, buffer: Bytes, addr: &SocketAddr) -> Self::SendTo;

    fn recv_from(self, buffer: BytesMut) -> Self::RecvFrom;
}

pub trait NetEventQueue: EventQueue {
    type TcpListener: TcpListener<EventQueue = Self, TcpStream = Self::TcpStream>;
    type TcpStream: TcpStream<EventQueue = Self>;
    type UdpSocket: UdpSocket<EventQueue = Self>;
}

#[cfg(test)]
macro_rules! make_net_tests {
    ($make_evloop:expr) => {
        #[allow(unused)]
        use ::queue::{EventQueue, Handle};
        use ::net::{TcpListener as GenTcpListener, TcpStream as GenTcpStream, UdpSocket as GenUdpSocket};

        use std::thread;
        use std::time::Duration;
        use std::net::{SocketAddr, SocketAddrV4, SocketAddrV6, Ipv4Addr};

        use futures::future;
        use net2::TcpBuilder;

        const MESSAGE: &str = "hello";

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
            let evloop = $make_evloop;
            let handle = evloop.handle();
            let listener = TcpListener::bind(&"0.0.0.0:0".parse().unwrap(), &handle).unwrap();
            let mut listener_addr = listener.local_addr().unwrap();
            listener_addr.set_ip("127.0.0.1".parse().unwrap());

            let server_thread = run_evloop_thread(&evloop, {
                listener.accept().and_then(|(server, _)| {
                    server
                        .read(BytesMut::from(vec![0u8; MESSAGE.len()]))
                        .and_then(|(server, buffer, _bytes)| server.write(buffer.freeze()))
                }).map(|_| ()).map_err(|err| panic!("{:?}", err))
            });

            thread::sleep(Duration::from_millis(10));

            let client_thread = run_evloop_thread(&evloop, future::lazy(move || {
                let connected = TcpStream::connect(&listener_addr, &handle);
                connected.and_then(|client| {
                    client
                        .write(Bytes::from(MESSAGE.to_string().into_bytes()))
                        .and_then(|(client, buffer, _bytes)| client.read(buffer.try_mut().unwrap()))
                })
            }));
            server_thread.join().unwrap().unwrap();
            client_thread.join().unwrap().unwrap();
        }

        #[test]
        fn udp_echo() {
            let mut evloop = $make_evloop;
            let handle = evloop.handle();
            let server = UdpSocket::bind(&"0.0.0.0:0".parse().unwrap(), &handle).unwrap();
            let mut server_addr = server.local_addr().unwrap();
            server_addr.set_ip("127.0.0.1".parse().unwrap());
            let client = UdpSocket::bind(&"0.0.0.0:0".parse().unwrap(), &handle).unwrap();

            let server_thread = run_evloop_thread(&evloop, {
                server
                    .recv_from(BytesMut::from(vec![0u8; MESSAGE.len()]))
                    .and_then(|(server, buffer, _bytes, remote_addr)| server.send_to(buffer.freeze(), &remote_addr))
                    .map(|_| ()).map_err(|err| panic!("{:?}", err))
            });

            thread::sleep(Duration::from_millis(10));

            evloop.run(future::lazy(|| {
                client
                    .send_to(Bytes::from(MESSAGE.to_string().into_bytes()), &server_addr)
                    .and_then(|(client, buffer)| client.recv_from(buffer.try_mut().unwrap()))
            })).unwrap();

            server_thread.join().unwrap().unwrap();
        }

        fn run_evloop_thread<E: EventQueue + Send + Clone + 'static, F: Future + Send + 'static>(evloop: &E, future: F) -> thread::JoinHandle<::std::result::Result<F::Item, F::Error>> where
            F::Item: Send, F::Error: Send,
        {
            let mut evloop = evloop.clone();
            thread::spawn(move || {
                evloop.run(future)
            })
        }
    }
} 