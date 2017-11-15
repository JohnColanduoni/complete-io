use ::queue::{EventQueue};
use ::evloop::EventLoop;
use ::io::*;

use std::io::{Error, Result};
use std::net::{SocketAddr, TcpListener as StdTcpListener, TcpStream as StdTcpStream, UdpSocket as StdUdpSocket};

use futures::prelude::*;
use bytes::{Bytes, BytesMut};

pub trait TcpListener: Sized + Clone + Send + 'static {
    type EventRegistrar: EventRegistrar;
    type TcpStream: TcpStream<EventRegistrar = Self::EventRegistrar>;

    type Accept: Future<Item = (Self::TcpStream, SocketAddr), Error = Error> + Send + 'static;

    fn bind<R: AsRegistrar<Self::EventRegistrar>>(addr: &SocketAddr, handle: &R) -> Result<Self> {
        Self::from_listener(StdTcpListener::bind(addr)?, handle)
    }

    fn from_listener<R: AsRegistrar<Self::EventRegistrar>>(listener: StdTcpListener, handle: &R) -> Result<Self>;

    fn accept(&self) -> Self::Accept;

    fn local_addr(&self) -> Result<SocketAddr>;
}

pub trait TcpStream: Sized + AsyncRead + AsyncWrite + Clone + Send + 'static {
    type EventRegistrar: EventRegistrar;

    type Connect: Future<Item = Self, Error = Error> + Send + 'static;

    fn connect<R: AsRegistrar<Self::EventRegistrar>>(addr: &SocketAddr, handle: &R) -> Self::Connect;

    /// Starts a connection operation on an unconnected `TcpStream` (i.e. one produced via `net2::TcpBuilder`).
    fn connect_with<R: AsRegistrar<Self::EventRegistrar>>(stream: StdTcpStream, addr: &SocketAddr, handle: &R) -> Self::Connect;

    fn from_stream<R: AsRegistrar<Self::EventRegistrar>>(stream: StdTcpStream, handle: &R) -> Result<Self>;

    fn local_addr(&self) -> Result<SocketAddr>;
}

pub trait UdpSocket: Sized + Clone + Send + 'static {
    type EventRegistrar: EventRegistrar;

    type SendTo: Future<Item = (Self, Bytes), Error = Error> + Send + 'static;
    type RecvFrom: Future<Item = (Self, BytesMut, usize, SocketAddr), Error = Error> + Send + 'static;
    
    fn bind<R: AsRegistrar<Self::EventRegistrar>>(addr: &SocketAddr, handle: &R) -> Result<Self> {
        Self::from_socket(StdUdpSocket::bind(addr)?, handle)
    }

    fn from_socket<R: AsRegistrar<Self::EventRegistrar>>(socket: StdUdpSocket, handle: &R) -> Result<Self>;

    fn local_addr(&self) -> Result<SocketAddr>;

    fn connect(&self, addr: &SocketAddr) -> Result<()>;

    fn send_to(self, buffer: Bytes, addr: &SocketAddr) -> Self::SendTo;

    fn recv_from(self, buffer: BytesMut) -> Self::RecvFrom;
}

pub trait NetEventQueue: EventQueue {
    type TcpListener: TcpListener<EventRegistrar = Self, TcpStream = Self::TcpStream>;
    type TcpStream: TcpStream<EventRegistrar = Self>;
    type UdpSocket: UdpSocket<EventRegistrar = Self>;
}

pub trait NetEventLoop: EventLoop {
    type TcpListener: TcpListener<EventRegistrar = Self::EventRegistrar, TcpStream = Self::TcpStream>;
    type TcpStream: TcpStream<EventRegistrar = Self::EventRegistrar>;
    type UdpSocket: UdpSocket<EventRegistrar = Self::EventRegistrar>;
}

#[cfg(test)]
pub(crate) mod gen_tests {
    use ::net::{NetEventLoop, TcpListener, TcpStream};
    use ::io::{AsyncRead, AsyncWrite};
    use ::evloop::{Handle, LocalHandle};

    use std::{str};
    use std::net::{ToSocketAddrs};

    use futures::future;
    use futures::prelude::*;
    use bytes::{Bytes, BytesMut};

    const MESSAGE: &str = "hello";

    pub fn tcp_echo<L: NetEventLoop>(mut evloop: L) {
        evloop.run(|handle| {
            let listener = L::TcpListener::bind(&"0.0.0.0:0".parse().unwrap(), handle).unwrap();
            let mut listener_addr = listener.local_addr().unwrap();
            listener_addr.set_ip("127.0.0.1".parse().unwrap());
            let client = L::TcpStream::connect(&listener_addr, handle);

            handle.global().spawn(move |_| {
                listener.accept().and_then(|(server, _)| {
                    server
                        .read(BytesMut::with_capacity(MESSAGE.len()))
                        .and_then(|(server, buffer, _bytes)| server.write(buffer.freeze()))
                }).map(|_| ()).map_err(|err| panic!("server error: {:?}", err))
            });

            future::lazy(move || {
                client.and_then(|client| {
                    client
                        .write(Bytes::from(MESSAGE.to_string().into_bytes()))
                        .and_then(|(client, buffer, _bytes)| client.read(buffer.try_mut().unwrap()))
                })
            })
        }).unwrap();
    }

    pub fn http_remote<L: NetEventLoop>(mut evloop: L) {
        let google_com_addr = "example.com:80".to_socket_addrs().unwrap().next().unwrap();

        let (_, buffer, _) = evloop.run(move |handle| {
            let request = Bytes::from_static(b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n");
            let buffer = BytesMut::with_capacity(8192);
            L::TcpStream::connect(&google_com_addr, handle)
                // TODO: write_all
                .and_then(|stream| stream.write(request))
                .and_then(|(stream, _, _)| stream.read(buffer))

        }).unwrap();

        assert!(str::from_utf8(&buffer).unwrap().starts_with("HTTP/1.1 200 OK\r\n"));
    }
}

#[cfg(test)]
pub(crate) mod gen_queue_tests {
    use ::net::{NetEventQueue, TcpStream};
    use ::io::{AsyncRead, AsyncWrite};

    use std::{str};
    use std::net::{ToSocketAddrs};

    use bytes::{Bytes, BytesMut};
    use futures::prelude::*;
    use futures::future;

    pub fn http_remote<Q: NetEventQueue>(evqueue: Q) {
        let google_com_addr = "example.com:80".to_socket_addrs().unwrap().next().unwrap();

        let handle = evqueue.handle();
        let (_, buffer, _) = evqueue.run(future::lazy(move || {
            let request = Bytes::from_static(b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n");
            let buffer = BytesMut::with_capacity(8192);
            Q::TcpStream::connect(&google_com_addr, &handle)
                // TODO: write_all
                .and_then(|stream| stream.write(request))
                .and_then(|(stream, _, _)| stream.read(buffer))
        })).unwrap();

        assert!(str::from_utf8(&buffer).unwrap().starts_with("HTTP/1.1 200 OK\r\n"));
    }
}


#[cfg(test)]
macro_rules! make_net_loop_tests {
    ($make_evloop:expr) => {
        #[test]
        fn tcp_echo() {
            ::net::gen_tests::tcp_echo($make_evloop);
        }

        #[test]
        fn http_remote() {
            ::net::gen_tests::http_remote($make_evloop);
        }
    }
} 

#[cfg(test)]
macro_rules! make_net_queue_tests {
    ($make_evqueue:expr) => {
        #[test]
        fn http_remote() {
            ::net::gen_queue_tests::http_remote($make_evqueue);
        }
    }
} 