#[macro_use] extern crate futures;

#[cfg(feature = "tokio")]
extern crate tokio_core;
#[cfg(feature = "tokio")]
extern crate mio;

#[cfg(all(feature = "iocp", target_os = "windows"))]
extern crate miow;
#[cfg(all(feature = "iocp", target_os = "windows"))]
extern crate winhandle;
#[cfg(all(feature = "iocp", target_os = "windows"))]
extern crate winapi;
#[cfg(all(feature = "iocp", target_os = "windows"))]
extern crate kernel32;
#[cfg(all(feature = "iocp", target_os = "windows"))]
extern crate ws2_32;
#[cfg(all(feature = "iocp", target_os = "windows"))]
extern crate net2;

pub mod evloop;
#[macro_use] pub mod net;
mod genio;

#[cfg(feature = "tokio")]
pub mod tokio;

#[cfg(all(feature = "iocp", target_os = "windows"))]
pub mod iocp;

pub use evloop::{EventLoop, ConcurrentEventLoop};
pub use genio::*;