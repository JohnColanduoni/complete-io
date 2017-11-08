use std::time::Duration;

use futures::prelude::*;

pub trait EventLoop: Sized {
    type LocalHandle: LocalHandle<EventLoop = Self> + AsRegistrar<Self>;
    type RemoteHandle: RemoteHandle<EventLoop = Self>;
    type Registrar: Registrar<EventLoop = Self>;

    fn handle(&self) -> Self::LocalHandle;

    fn remote(&self) -> Self::RemoteHandle;

    /// Runs a future on the current thread, driving the event loop while we're otherwise waiting
    /// for the future to complete.
    /// 
    /// The future will never leave either the current thread or stack frame, so it need not implement `Send` (or `'static`).
    /// 
    /// This method only allows one thread to run the event loop even on `ConcurrentEventLoop`s. For those you should use
    /// `run_local` or `run_concurrent` unless this is required.
    fn run<F>(&mut self, f: F) -> Result<F::Item, F::Error> where
        F: Future;

    /// Performs one iteration of the event loop, blocking on waiting for events for at most `max_wait` (forever if `None`).
    /// 
    /// This method only allows one thread to run the event loop even on `ConcurrentEventLoop`s. For those you should use
    /// `turn_concurrent` unless this is required.
    fn turn(&mut self, max_wait: Option<Duration>);
}

pub trait ConcurrentEventLoop: EventLoop + Send + Sync where
    <Self as EventLoop>::Registrar: Send,
    <Self as EventLoop>::RemoteHandle: AsRegistrar<Self>,
{
    /// Runs a future on the event loop, driving the event loop while we're otherwise waiting
    /// for the future to complete.
    /// 
    /// The future will not leave this stack frame, but it may be polled on other threads.
    /// As such it must be `Send` but need not be `'static`.
    fn run_concurrent<F>(&self, f: F) -> Result<F::Item, F::Error> where
        F: Future + Send;

    /// Performs one iteration of the event loop, blocking on waiting for events for at most `max_wait` (forever if `None`).
    /// 
    /// Other threads may drive the event loop along with this one.
    fn turn_concurrent(&self, max_wait: Option<Duration>);

    /// Runs a function producing a future on the current thread, driving the event loop while we're otherwise waiting
    /// for the future to complete.
    /// 
    /// Neither the closure not the future will ever leave either the current thread or stack frame, so they need 
    /// not implement `Send` (or `'static`). Unlike `run`, other threads may drive the event loop at the same time.
    fn run_local<F, R>(&self, f: F) -> Result<R::Item, R::Error> where
        F: FnOnce(&Self::LocalHandle) -> R,
        R: IntoFuture;
}

/// An `EventLoop` handle which is locked to a specific EventLoop thread.
/// 
/// All futures spawned onto the EventLoop will never leave the current thread, which
/// allows them to not implement `Send`. If this restriction is not required, you may
/// want to use `RemoteHandle` which may be more efficient on a multithreaded event loops.
pub trait LocalHandle: Sized + Clone + 'static {
    type EventLoop: EventLoop;

    fn remote(&self) -> &<Self::EventLoop as EventLoop>::RemoteHandle;

    fn spawn<F>(&self, f: F) where
        F: Future<Item = (), Error = ()> + 'static;

    fn spawn_fn<F, R>(&self, f: F) where
        F: FnOnce() -> R + 'static,
        R: IntoFuture<Item = (), Error = ()> + 'static;
}

/// An `EventLoop` handle which is free to migrate to diffent specific EventLoop threads.
/// 
/// All futures spawned onto the EventLoop will never leave the current thread, which
/// allows them to not implement `Send`. If this restriction is not required, you may
/// want to use `RemoteHandle` which may be more efficient on a multithreaded event loops.
pub trait RemoteHandle: Sized + Clone + Send + 'static {
    type EventLoop: EventLoop;

    /// Spawns a function that produces a future onto an `EventLoop` thread. The future will
    /// always be polled on the initial thread, so it need not implement `Send`.
    fn spawn_locked<F, R>(&self, f: F) where
        F: FnOnce(&<Self::EventLoop as EventLoop>::LocalHandle) -> R + Send + 'static,
        R: IntoFuture<Item = (), Error = ()>,
        R::Future: 'static;

    /// Spawns a future onto an `EventLoop` thread. The future
    /// may be polled on different threads so it must implement `Send`.
    fn spawn<F>(&self, f: F) where
        F: Future<Item = (), Error = ()> + Send + 'static;

    /// Spawns a function that produces a future onto an `EventLoop` thread. The future
    //// may be polled on different threads to it must implement `Send`.
    fn spawn_fn<F, R>(&self, f: F) where
        F: FnOnce(&<Self::EventLoop as EventLoop>::LocalHandle) -> R + Send + 'static,
        R: IntoFuture<Item = (), Error = ()>,
        R::Future: Send + 'static;
}

/// An `EventLoop` handle which can be used to register IO objects in the event loop.
pub trait Registrar: Sized + Clone + 'static {
    type EventLoop: EventLoop;
}

pub trait AsRegistrar<EL: EventLoop> {
    fn as_registrar(&self) -> &EL::Registrar;
}