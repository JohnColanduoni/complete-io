use std::{io, mem};
use std::io::Error;

use futures::prelude::*;
use bytes::{Bytes, BytesMut};

pub trait AsyncRead: Sized {
    type Read: Future<Item = (Self, BytesMut, usize), Error = Error> + Send + 'static;

    fn read(self, buffer: BytesMut) -> Self::Read;
}

pub trait AsyncWrite: Sized {
    type Write: Future<Item = (Self, Bytes, usize), Error = Error> + Send + 'static;

    fn write(self, buffer: Bytes) -> Self::Write;
}
