pub mod threadpool;

#[cfg(feature = "tokio")]
pub mod tokio;

use ::io::{EventRegistrar, AsRegistrar};

use std::time::Duration;

use futures::prelude::*;

pub trait EventLoop: Sized {
    type Handle: Handle<EventLoop = Self>;
    type LocalHandle: LocalHandle<EventLoop = Self> + AsRegistrar<Self::EventRegistrar>;
    type EventRegistrar: EventRegistrar;

    fn handle(&self) -> <Self as EventLoop>::Handle;

    /// Gets a thread-specific handle to the `EventLoop`.
    /// 
    /// This function will only return a handle if the current thread is a worker
    /// for the `EventLoop`.
    fn local_handle(&self) -> Option<Self::LocalHandle>;

    fn run_future<F: Future>(&mut self, f: F) -> Result<F::Item, F::Error>;

    fn run<F, R>(&mut self, f: F) -> Result<R::Item, R::Error> where
        F: FnOnce(&Self::LocalHandle) -> R + Send + 'static,
        R: IntoFuture,
        R::Future: Send + 'static,
        R::Item: Send + 'static, R::Error: Send + 'static;
}

pub trait ConcurrentEventLoop: EventLoop + Send + Sync {
}

/// A `ConcurrentEventLoop` that allows arbitrary threads to join it as workers temporarily.
pub trait FlexibleEventLoop: EventLoop {
    fn turn(&mut self, max_wait: Option<Duration>);

    fn run<F>(&mut self, f: F) -> Result<F::Item, F::Error> where
        F: Future;
}

pub trait ConcurrentFlexibleEventLoop: ConcurrentEventLoop {
    fn turn_concurrent(&self, max_wait: Option<Duration>);

    fn run_concurrent<F>(&self, f: F) -> Result<F::Item, F::Error> where
        F: Future + Send;
}

pub trait Handle: Sized + Send + Clone + 'static {
    type EventLoop: EventLoop;

    /// Gets a thread-specific handle to the `EventLoop`.
    /// 
    /// This function will only return a handle if the current thread is a worker
    /// for the `EventLoop`.
    fn local(&self) -> Option<<Self::EventLoop as EventLoop>::LocalHandle>;

    fn spawn<F, R>(&self, f: F) where
        F: FnOnce(&<Self::EventLoop as EventLoop>::LocalHandle) -> R + Send + 'static,
        R: IntoFuture<Item = (), Error = ()>,
        R::Future: Send + 'static;

    /// Spawns a future on a random worker of the `EventLoop` and locks it on that thread.
    fn spawn_locked<F, R>(&self, f: F) where
        F: FnOnce(&<Self::EventLoop as EventLoop>::LocalHandle) -> R + Send + 'static,
        R: IntoFuture<Item = (), Error = ()>,
        R::Future: 'static;

    fn spawn_future<F>(&self, f: F) where
        F: Future<Item = (), Error = ()> + Send + 'static;
}

pub trait LocalHandle: Sized {
    type EventLoop: EventLoop;

    /// Gets the underlying global `EventLoop` handle which is not locked tp a specific thread
    fn global(&self) -> &<Self::EventLoop as EventLoop>::Handle;

    fn spawn_local<F, R>(&self, f: F) where
        F: FnOnce(&Self) -> R + 'static,
        R: IntoFuture<Item = (), Error = ()>,
        R::Future: 'static;

    fn spawn_local_future<F>(&self, f: F) where
        F: Future<Item = (), Error = ()> + 'static;
}
