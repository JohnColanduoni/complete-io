use std::{io, mem};
use std::io::Error;

use futures::prelude::*;

pub trait AsyncRead: Sized {
    #[doc(hidden)]
    type FutureImpl: IoReadFutureImpl<Item = (Self, usize)>;

    fn read<B: LockedBufferMut>(self, b: B) -> Read<B, Self::FutureImpl>;
}

pub trait AsyncWrite: Sized {
    #[doc(hidden)]
    type FutureImpl: IoWriteFutureImpl<Item = (Self, usize)>;

    fn write<B: LockedBuffer>(self, b: B) -> Write<B, Self::FutureImpl>;
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

#[must_use = "futures do nothing unless polled"]
pub struct Read<B, F: IoReadFutureImpl> {
    #[doc(hidden)]
    pub buffer: Option<B>,
    #[doc(hidden)]
    pub future: F,
}

#[must_use = "futures do nothing unless polled"]
pub struct Write<B, F: IoWriteFutureImpl> {
    #[doc(hidden)]
    pub buffer: Option<B>,
    #[doc(hidden)]
    pub future: F,
}

impl<B, F: IoReadFutureImpl> Drop for Read<B, F> {
    fn drop(&mut self) {
        if let Some(buffer) = self.buffer.take() {
            if let Err(err) = self.future.cancel() {
                warn!("leaking buffer because cancellation of IO operation failed: {:?}", err);
                mem::forget(buffer);
            }
        }
    }
}

impl<B, F: IoWriteFutureImpl> Drop for Write<B, F> {
    fn drop(&mut self) {
        if let Some(buffer) = self.buffer.take() {
            if let Err(err) = self.future.cancel() {
                warn!("leaking buffer because cancellation of IO operation failed: {:?}", err);
                mem::forget(buffer);
            }
        }
    }
}

#[doc(hidden)]
pub trait IoFutureImpl {
    /// Tries to cancel the operation. This is needed in completion-based implementations, where
    /// the buffer may still be in use.
    fn cancel(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// This is an unfortune kludge required because generic functions in traits
/// can not return values whose type depends on generic types at the function level.
#[doc(hidden)]
pub trait IoReadFutureImpl: IoFutureImpl {
    type Item: Sized;

    fn poll<B: LockedBufferMut>(&mut self, buffer: &mut B) -> Poll<Self::Item, Error>;
}

/// This is an unfortune kludge required because generic functions in traits
/// can not return values whose type depends on generic types at the function level.
#[doc(hidden)]
pub trait IoWriteFutureImpl: IoFutureImpl {
    type Item: Sized;

    fn poll<B: LockedBuffer>(&mut self, b: &B) -> Poll<Self::Item, Error>;
}

impl<B, F, T1: Sized, T2: Sized> Future for Read<B, F> where
    B: LockedBufferMut,
    F: IoReadFutureImpl<Item = (T1, T2)>,
{
    type Item = (T1, B, T2);
    type Error = Error;

    fn poll(&mut self) -> Poll<(T1, B, T2), Error> {
        let (t1, t2) = match self.buffer {
            Some(ref mut buffer) => {
                try_ready!(self.future.poll(buffer))
            },
            None => panic!("future already completed"),
        };

        Ok(Async::Ready((t1, self.buffer.take().unwrap(), t2)))
    }
}

impl<B, F, T1: Sized, T2: Sized> Future for Write<B, F> where
    B: LockedBuffer,
    F: IoWriteFutureImpl<Item = (T1, T2)>,
{
    type Item = (T1, B, T2);
    type Error = Error;

    fn poll(&mut self) -> Poll<(T1, B, T2), Error> {
        let (t1, t2) = match self.buffer {
            Some(ref buffer) => {
                try_ready!(self.future.poll(buffer))
            },
            None => panic!("future already completed"),
        };

        Ok(Async::Ready((t1, self.buffer.take().unwrap(), t2)))
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