use ::io::{EventRegistrar, AsRegistrar};

use futures::prelude::*;
use futures::future;
use tokio_core::reactor::{Core, Handle, Remote};

impl super::EventLoop for Core {
    type Handle = Remote;
    type LocalHandle = Handle;
    type EventRegistrar = Self;

    fn handle(&self) -> Remote { self.remote() }

    fn local_handle(&self) -> Option<Self::LocalHandle> {
        Some(self.handle())
    }

    fn run_future<F: Future>(&mut self, f: F) -> Result<F::Item, F::Error> {
        Core::run(self, f)
    }

    fn run<F, R>(&mut self, f: F) -> Result<R::Item, R::Error> where
        F: FnOnce(&Self::LocalHandle) -> R + Send + 'static,
        R: IntoFuture,
        R::Future: Send + 'static,
        R::Item: Send + 'static, R::Error: Send + 'static,
    {
        let handle = Core::handle(self);
        Core::run(self, future::lazy(|| {
            f(&handle)
        }))
    }
}

impl EventRegistrar for Core {
    type RegHandle = Handle;
}

impl super::Handle for Remote {
    type EventLoop = Core;

    fn local(&self) -> Option<Handle> {
        self.handle()
    }

    fn spawn<F, R>(&self, f: F) where
        F: FnOnce(&Handle) -> R + Send + 'static,
        R: IntoFuture<Item = (), Error = ()>,
        R::Future: Send + 'static,
    {
        self.spawn(f)
    }

    fn spawn_locked<F, R>(&self, f: F) where
        F: FnOnce(&Handle) -> R + Send + 'static,
        R: IntoFuture<Item = (), Error = ()>,
        R::Future: 'static,
    {
        self.spawn(f)
    }

    fn spawn_future<F>(&self, _f: F) where
        F: Future<Item = (), Error = ()> + Send + 'static,
    {
        unimplemented!()
    }
}

impl super::LocalHandle for Handle {
    type EventLoop = Core;

    fn global(&self) -> &Remote { self.remote() }

    fn spawn_local<F, R>(&self, f: F) where
        F: FnOnce(&Self) -> R + 'static,
        R: IntoFuture<Item = (), Error = ()>,
        R::Future: 'static,
    {
        let handle = self.clone();
        self.spawn_fn(move || f(&handle).into_future())
    }

    fn spawn_local_future<F>(&self, f: F) where
        F: Future<Item = (), Error = ()> + 'static,
    {
        self.spawn(f)
    }
}

impl AsRegistrar<Core> for Handle {
    fn as_registrar(&self) -> &Self { self }
}