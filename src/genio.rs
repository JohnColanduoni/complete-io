use std::{io};
use std::io::Error;
use std::sync::Arc;

use futures::prelude::*;

pub trait AsyncRead: Sized {
    type FutureImpl: ReadFutureImpl<Io = Self>;

    fn read<B: LockedBufferMut>(self, b: B) -> Read<B, Self::FutureImpl>;
}

pub trait AsyncWrite: Sized {
    type FutureImpl: WriteFutureImpl<Io = Self>;

    fn write<B: LockedBuffer>(self, b: B) -> Write<B, Self::FutureImpl>;
}


#[must_use = "futures do nothing unless polled"]
pub struct Read<B, F> {
    #[doc(hidden)]
    pub buffer: Option<B>,
    #[doc(hidden)]
    pub future: F,
}

#[must_use = "futures do nothing unless polled"]
pub struct Write<B, F> {
    #[doc(hidden)]
    pub buffer: Option<B>,
    #[doc(hidden)]
    pub future: F,
}

/// A marker trait that indicates a buffer type will always return
/// the same pointer and the data may be read at any time.
/// 
/// These guarantees are needed by some async IO implementations to
/// operate without copies.
pub unsafe trait LockedBuffer: AsRef<[u8]> {
}
/// A marker trait that indicates a buffer type will always return
/// the same pointer and that the buffer may be written to at any
/// time.
/// 
/// These guarantees are needed by some async IO implementations to
/// operate without copies.
pub unsafe trait LockedBufferMut: AsMut<[u8]> {
}

#[doc(hidden)]
/// This is an unfortune kludge required because generic functions in traits
/// can not return values whose type depends on generic types at the function level.
pub trait ReadFutureImpl {
    type Io: Sized;

    fn poll<B: LockedBufferMut>(&mut self, b: &mut B) -> Poll<(Self::Io, usize), Error>;
}

#[doc(hidden)]
/// This is an unfortune kludge required because generic functions in traits
/// can not return values whose type depends on generic types at the function level.
pub trait WriteFutureImpl {
    type Io: Sized;

    fn poll<B: LockedBuffer>(&mut self, b: &B) -> Poll<(Self::Io, usize), Error>;
}

impl<B, F> Future for Read<B, F> where
    B: LockedBufferMut,
    F: ReadFutureImpl,
{
    type Item = (F::Io, B, usize);
    type Error = Error;

    fn poll(&mut self) -> Poll<(F::Io, B, usize), Error> {
        let (io, bytes) = match self.buffer {
            Some(ref mut buffer) => {
                try_ready!(self.future.poll(buffer))
            },
            None => panic!("future already completed"),
        };

        Ok(Async::Ready((io, self.buffer.take().unwrap(), bytes)))
    }
}

impl<B, F> Future for Write<B, F> where
    B: LockedBuffer,
    F: WriteFutureImpl,
{
    type Item = (F::Io, B, usize);
    type Error = Error;

    fn poll(&mut self) -> Poll<(F::Io, B, usize), Error> {
        let (io, bytes) = match self.buffer {
            Some(ref buffer) => {
                try_ready!(self.future.poll(buffer))
            },
            None => panic!("future already completed"),
        };

        Ok(Async::Ready((io, self.buffer.take().unwrap(), bytes)))
    }
}

unsafe impl LockedBuffer for Vec<u8> {
}
unsafe impl LockedBufferMut for Vec<u8> {
}
unsafe impl LockedBuffer for Box<[u8]> {
}
unsafe impl LockedBufferMut for Box<[u8]> {
}