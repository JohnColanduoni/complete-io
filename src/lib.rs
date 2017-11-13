extern crate futures;
#[macro_use] extern crate log;
extern crate bytes;
#[macro_use] extern crate cfg_if;

cfg_if! { if #[cfg(all(feature = "iocp", target_os = "windows"))] {
    extern crate miow;
    #[macro_use] extern crate winhandle;
    extern crate winapi;
    extern crate kernel32;
    extern crate ws2_32;
    extern crate net2;
} }

pub mod queue;
#[macro_use] pub mod net;
pub mod io;

#[cfg(all(feature = "iocp", target_os = "windows"))]
pub mod iocp;

pub use queue::{EventQueue};
pub use io::{AsyncRead, AsyncWrite};