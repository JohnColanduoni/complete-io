use ::io::{EventRegistrar, AsRegistrar};

use std::{io};
use std::time::Duration;
use std::hash::Hash;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::prelude::*;
use futures::executor::{self, Notify};

pub trait EventQueue: Sized + Send + Sync + 'static + EventRegistrar<RegHandle = <Self as EventQueue>::Handle> {
    type Handle: Handle<EventQueue = Self> + AsRegistrar<Self>;
    type EventTag: EventTag<EventQueue = Self>;
    type Interrupt: Interrupt<EventQueue = Self>;

    fn handle(&self) -> <Self as EventQueue>::Handle;

    /// Performs one iteration of the event loop, blocking on waiting for events for at most `max_wait` (unbounded if `None`).
    fn turn(&self, max_wait: Option<Duration>) -> io::Result<usize>;

    /// Performs one iteration of the event loop, blocking on waiting for events for at most `max_wait` (unbounded if `None`).
    /// 
    /// Allows interruptions via an `Interrupt`.
    /// 
    /// Calling this function with the same interrupt at the same time results in a race condition.
    fn turn_interruptible(&self, max_wait: Option<Duration>, interrupt: &Self::Interrupt) -> io::Result<usize>;

    fn new_custom_event(&self, handler: CustomEventHandler<Self>) -> io::Result<Self::EventTag>;

    fn run<F>(&self, future: F) -> Result<F::Item, F::Error> where
        F: Future,
    {
        let mut task = executor::spawn(future);
        let notify = InterruptNotify::<Self>::new();
        loop {
            if notify.reset() {
                match task.poll_future_notify(&notify, 0)? {
                    Async::Ready(x) => return Ok(x),
                    Async::NotReady => {},
                }
            }
            self.turn_interruptible(None, notify.interrupt()).expect("failed to turn EventQueue while driving future");
        }
    }
}

pub type CustomEventHandler<E> = Box<Fn(<E as EventQueue>::EventTag, usize) + Send + Sync>;

pub trait Handle: Sized + Clone + Send + Sync + 'static {
    type EventQueue: EventQueue;

    /// Posts a user-defined event
    fn post_custom_event(&self, tag: <Self::EventQueue as EventQueue>::EventTag, data: usize) -> io::Result<()>;
}

pub trait EventTag: Sized + Send + Sync + Clone + Copy + PartialEq + Eq + Hash + 'static {
    type EventQueue: EventQueue;
}

pub trait Interrupt: Sized + Send + Sync + Clone + 'static {
    type EventQueue: EventQueue;

    fn new() -> Self;

    fn interrupt(&self) -> io::Result<bool>;
}

pub struct InterruptNotify<Q: EventQueue> {
    interrupt: Q::Interrupt,
    notified: AtomicBool,
}

impl<Q: EventQueue> InterruptNotify<Q> {
    pub fn new() -> Arc<Self> {
        Arc::new(InterruptNotify {
            interrupt: Q::Interrupt::new(),
            notified: AtomicBool::new(true),
        })
    }

    pub fn interrupt(&self) -> &Q::Interrupt { &self.interrupt }

    pub fn reset(&self) -> bool {
        // TODO: weaken?
        self.notified.swap(false, Ordering::SeqCst)
    }
}

impl<Q: EventQueue> Notify for InterruptNotify<Q> {
    fn notify(&self, _id: usize) {
        if !self.notified.swap(true, Ordering::SeqCst) { // TODO: weaken?
            if let Err(error) = self.interrupt.interrupt() {
                warn!("notification via interrupt of EventQueue thread failed: {}", error);
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod gen_tests {
    use ::queue::{EventQueue, Interrupt};

    use std::{thread};
    use std::sync::{Arc, Barrier};

    use futures;
    use futures::prelude::*;

    pub fn interrupt_thread<Q: EventQueue>(evqueue: Q) {
        let evqueue = Arc::new(evqueue);
        let interrupt = Q::Interrupt::new();
        let (tx, rx) = futures::sync::oneshot::channel();
        let barrier = Arc::new(Barrier::new(2));
        let thread = thread::spawn({
            let evqueue = evqueue.clone();
            let interrupt = interrupt.clone();
            let barrier = barrier.clone();
            move || {
                barrier.wait();
                evqueue.turn_interruptible(None, &interrupt).unwrap();
                let _ = tx.send(());
            }
        });
        barrier.wait();
        let interrupted = interrupt.interrupt().unwrap();
        assert!(interrupted);
        rx.wait().unwrap();

        thread.join().unwrap();
    }
}

#[cfg(test)]
macro_rules! make_evqueue_tests {
    ($make_evqueue:expr) => {
        #[test]
        fn interrupt_thread() {
            ::queue::gen_tests::interrupt_thread($make_evqueue);
        }
    };
}