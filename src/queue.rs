use std::{io};
use std::time::Duration;
use std::hash::Hash;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::prelude::*;
use futures::executor::{self, Notify};

pub trait EventQueue: Sized {
    type Handle: Handle<EventQueue = Self>;
    type EventTag: EventTag<EventQueue = Self>;

    fn handle(&self) -> Self::Handle;

    /// Performs one iteration of the event loop, blocking on waiting for events for at most `max_wait` (unbounded if `None`).
    fn turn(&mut self, max_wait: Option<Duration>) -> io::Result<usize>;

    fn new_custom_event(&self, handler: CustomEventHandler<Self>) -> io::Result<Self::EventTag>;

    fn run<F>(&mut self, future: F) -> Result<F::Item, F::Error> where
        F: Future,
    {
        let mut task = executor::spawn(future);
        let simple_notify = Arc::new(SimpleNotify { notified: AtomicBool::new(true) });
        loop {
            if simple_notify.notified.swap(false, Ordering::SeqCst) { // TODO: relax ordering?
                match task.poll_future_notify(&simple_notify, 0)? {
                    Async::Ready(x) => return Ok(x),
                    Async::NotReady => {},
                }
            }
            self.turn(None).expect("failed to turn EventQueue while driving future");
        }
    }
}

pub type CustomEventHandler<E> = Box<Fn(<E as EventQueue>::EventTag, usize) + Send + Sync>;

pub trait Handle: Sized + Clone + Send + Sync {
    type EventQueue: EventQueue;

    /// Posts a user-defined event
    fn post_custom_event(&self, tag: <Self::EventQueue as EventQueue>::EventTag, data: usize) -> io::Result<()>;
}

pub trait EventTag: Sized + Send + Sync + Clone + Copy + PartialEq + Eq + Hash {
    type EventQueue: EventQueue;
}

struct SimpleNotify {
    notified: AtomicBool,
}

impl Notify for SimpleNotify {
    fn notify(&self, _id: usize) {
        self.notified.store(true, Ordering::SeqCst); // TODO: relax ordering?
    }
}