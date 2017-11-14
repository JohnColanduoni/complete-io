use std::io::Error;

use futures::prelude::*;
use bytes::{Bytes, BytesMut};

/// An entity capable of registering IO handles for event subscription.
/// 
/// This is implemented by both `EventLoop`s and `EventQueue`s. The former is
/// recommended unless you need a lower-level interface.
pub trait EventRegistrar {
    type RegHandle: Sized;
}

pub trait AsRegistrar<R: EventRegistrar> {
    fn as_registrar(&self) -> &R::RegHandle;
}

pub trait AsyncRead: Sized {
    type Read: Future<Item = (Self, BytesMut, usize), Error = Error> + Send + 'static;

    fn read(self, buffer: BytesMut) -> Self::Read;
}

pub trait AsyncWrite: Sized {
    type Write: Future<Item = (Self, Bytes, usize), Error = Error> + Send + 'static;

    fn write(self, buffer: Bytes) -> Self::Write;
}
