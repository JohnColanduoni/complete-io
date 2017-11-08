use std::{io};

use futures::prelude::*;

pub trait AsyncWrite<B: AsRef<[u8]> + Sized> {
    type Future: Future<Item = B, Error = io::Error>;

    fn write(&self, b: B) -> Self::Future;
}

pub trait AsyncRead<B: AsMut<[u8]> + Sized> {
    type Future: Future<Item = (B, usize), Error = io::Error>;

    fn read(&self, b: B) -> Self::Future;
}