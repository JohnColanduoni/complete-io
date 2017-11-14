extern crate futures;
extern crate net2;
#[macro_use] extern crate log;
extern crate bytes;
extern crate crossbeam;
extern crate coco;
extern crate num_cpus;
#[macro_use] extern crate bitflags;
#[macro_use] extern crate cfg_if;

cfg_if! { if #[cfg(all(feature = "iocp", target_os = "windows"))] {
    extern crate miow;
    #[macro_use] extern crate winhandle;
    extern crate winapi;
    extern crate kernel32;
    extern crate ws2_32;
} }

cfg_if! { if #[cfg(all(feature = "epoll", target_os = "linux"))] {
    extern crate libc;
} }

cfg_if! { if #[cfg(feature = "tokio")] {
    extern crate tokio_core;
} }

pub mod queue;
#[macro_use] pub mod net;
pub mod io;
pub mod evloop;

#[cfg(all(feature = "iocp", target_os = "windows"))]
pub mod iocp;

#[cfg(all(feature = "epoll", target_os = "linux"))]
pub mod epoll;

pub use evloop::{EventLoop};
pub use io::{AsyncRead, AsyncWrite};
