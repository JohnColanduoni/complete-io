pub mod net;

use ::queue::{self, EventQueue, CustomEventHandler};

use std::{io, mem};
use std::cell::UnsafeCell;
use std::sync::{Arc, Weak, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::os::unix::prelude::*;

use futures::prelude::*;
use futures::task::AtomicTask;
use libc;

#[derive(Clone)]
pub struct Epoll {
    inner: Arc<Inner>,
}

#[derive(Clone)]
pub struct Handle {
    inner: Arc<Inner>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct EventTag();

struct Inner {
    fd: RawFd,
}

impl Drop for Inner {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd); }
    }
}

bitflags! {
    pub struct EventMask: u32 {
        const READ = libc::EPOLLIN as u32;
        const WRITE = libc::EPOLLOUT as u32;
        const READ_HUP = libc::EPOLLRDHUP as u32;
        const EXCEPTIONAL = libc::EPOLLPRI as u32;
        const ERROR = libc::EPOLLERR as u32;
        const HUP = libc::EPOLLHUP as u32;
    }
}

bitflags! {
    pub struct EventMode: u32 {
        const EDGE = libc::EPOLLET as u32;
        const ONE_SHOT = libc::EPOLLONESHOT as u32;
    }
}

const EVENT_BUFFER_SIZE: usize = 32;

impl Epoll {
    pub fn new() -> io::Result<Self> {
        let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if fd < 0 {
            let error = io::Error::last_os_error();
            error!("epoll_create1 failed: {}", error);
            return Err(error);
        }

        Ok(Epoll { inner: Arc::new(Inner {
            fd,
        })})
    }

}

impl EventQueue for Epoll {
    type Handle = Handle;
    type EventTag = EventTag;

    fn handle(&self) -> Handle { Handle { inner: self.inner.clone(), } }

    fn turn(&mut self, max_wait: Option<Duration>) -> io::Result<usize> {
        let timeout_ms = if let Some(timeout) = max_wait {
            (timeout.as_secs() as u32 * 1000 + (timeout.subsec_nanos() / 1_000_000)) as i32
        } else {
            -1
        };

        loop {
            unsafe {
                let mut events: [libc::epoll_event; EVENT_BUFFER_SIZE] = mem::zeroed();
                let event_count = match libc::epoll_wait(self.inner.fd, events.as_mut_ptr(), events.len() as _, timeout_ms) {
                    -1 => {
                        let error = io::Error::last_os_error();
                        if error.kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        error!("epoll_wait failed: {}", error);
                        return Err(error);
                    },
                    0 => return Ok(0),
                    n => n as usize,
                };

                for event in events[0..event_count].iter() {
                    // Don't drop weak pointer since it may still be in epoll
                    let weak = mem::ManuallyDrop::new(mem::transmute::<_, Weak<_Registration>>(event.u64 as usize));
                    if let Some(registration) = weak.upgrade() {
                        let registration = Registration(registration);
                        registration.0.handler.handle_event(&registration, EventMask::from_bits_truncate(event.events));
                    }
                }

                return Ok(event_count);
            }
        }
    }

    fn new_custom_event(&self, handler: CustomEventHandler<Self>) -> io::Result<Self::EventTag> {
        unimplemented!()
    }
}

impl Handle {
    pub fn register_fd<F: AsRawFd + ?Sized>(&self, fd: &F, event_mask: EventMask, mode: EventMode, handler: Arc<RegistrationHandler>) -> io::Result<Registration> {
        let registration = Registration(Arc::new(_Registration {
            fd: fd.as_raw_fd(),
            queue: self.inner.clone(),
            epoll_data: 0.into(),
            handler,
        }));
        let mut event = libc::epoll_event {
            events: event_mask.bits() | mode.bits(),
            u64: unsafe { mem::transmute::<Weak<_Registration>, usize>(Arc::downgrade(&registration.0)) as u64 },
        };
        unsafe { *registration.0.epoll_data.get() = event.u64; }
        if unsafe { libc::epoll_ctl(self.inner.fd, libc::EPOLL_CTL_ADD, fd.as_raw_fd(), &mut event) } == -1 {
            // Don't EPOLL_CTL_DEL
            unsafe { mem::drop(mem::transmute::<_, Weak<_Registration>>(event.u64)) };
            mem::forget(Arc::try_unwrap(registration.0).unwrap_or_else(|_| unreachable!()));
            let error = io::Error::last_os_error();
            error!("adding via epoll_ctl failed: {}", error);
            return Err(error);
        } else {
            Ok(registration)
        }
    }
}

#[derive(Clone)]
pub struct Registration(Arc<_Registration>);

struct _Registration {
    fd: RawFd,
    // Contains the representation of a weak pointer to _Registration
    epoll_data: UnsafeCell<u64>,
    queue: Arc<Inner>,
    handler: Arc<RegistrationHandler>,
}

unsafe impl Send for _Registration {}
unsafe impl Sync for _Registration {}

pub trait RegistrationHandler {
    fn handle_event(&self, registration: &Registration, event: EventMask);
}

impl Drop for _Registration {
    fn drop(&mut self) {
        // Kernels prior to 2.6.9 don't like null pointer for event
        if unsafe { libc::epoll_ctl(self.queue.fd, libc::EPOLL_CTL_DEL, self.fd, 1usize as _) } == -1 {
            let error = io::Error::last_os_error();
            error!("unregistration via epoll_ctl failed: {}", error);
        } else {
            // epoll_event is gone, we can drop the weak reference
            unsafe { mem::drop(mem::transmute::<_, Weak<_Registration>>(*self.epoll_data.get() as usize)); }
        }
    }
}

impl Registration {
    pub fn modify(&self, event_mask: EventMask, mode: EventMode) -> io::Result<()> {
        let mut event = libc::epoll_event {
            events: event_mask.bits() | mode.bits(),
            u64: unsafe { *self.0.epoll_data.get() },
        };
        if unsafe { libc::epoll_ctl(self.0.queue.fd, libc::EPOLL_CTL_MOD, self.0.fd, &mut event) } == -1 {
            let error = io::Error::last_os_error();
            error!("modification via epoll_ctl failed: {}", error);
            return Err(error);
        } else{
            Ok(())
        }
    }
}

pub struct FuturesRegistration {
    registration: Registration,
    inner: Arc<FuturesRegistrationHandler>,
}

pub struct FutureInterest(Arc<_FutureInterest>);

struct _FutureInterest {
    task: AtomicTask,
    fired: AtomicBool,
}

struct FuturesRegistrationHandler {
    interests: Mutex<Vec<(EventMask, Arc<_FutureInterest>)>>,
}

impl FuturesRegistration {
    pub fn new<F: AsRawFd + ?Sized>(handle: &Handle, fd: &F) -> io::Result<FuturesRegistration> {
        let inner = Arc::new(FuturesRegistrationHandler {
            interests: Default::default(),
        });
        let registration = handle.register_fd(fd, EventMask::empty(), EventMode::EDGE | EventMode::ONE_SHOT, inner.clone())?;
        Ok(FuturesRegistration {
            registration, inner,
        })
    }

    // TODO: more efficient handling of interests, associated churn (should be lock-free, pool instances)
    pub fn add_interest(&self, events: EventMask) -> io::Result<FutureInterest> {
        let mut interests = self.inner.interests.lock().unwrap();
        let old_event_mask = interests.iter().fold(EventMask::empty(), |mask, &(interest_mask, _)| mask | interest_mask);
        let inner = Arc::new(_FutureInterest {
            task: AtomicTask::new(),
            fired: AtomicBool::new(false),
        });
        inner.task.register();
        interests.push((events, inner.clone()));
        if !old_event_mask.contains(events) {
            self.registration.modify(old_event_mask | events, EventMode::EDGE | EventMode::ONE_SHOT)?;
        }

        Ok(FutureInterest(inner))
    }
}

impl FutureInterest {
    /// Registers the current task to be notified for this interest.
    ///
    /// The behavior is the same as that of `AtomicTask::register`. Note that this need not be called
    /// immediately after `FuturesRegistration::add_interest` (the calling `Task` will already be registered).
    pub fn register(&self) {
        self.0.task.register();
    }

    pub fn fired(&self) -> bool {
        self.0.fired.load(Ordering::Relaxed)
    }

    pub fn poll(&self) -> Async<()> {
        if self.fired() {
            Async::Ready(())
        } else {
            self.register();
            Async::NotReady
        }
    }
}

impl RegistrationHandler for FuturesRegistrationHandler {
    // TODO: more efficient handling of interests, associated churn (should be lock-free, pool instances)
    fn handle_event(&self, registration: &Registration, events: EventMask) {
        let mut interests = self.interests.lock().unwrap();
        let mut i = 0;
        let mut remaining_interest_mask = None;
        loop {
            if let Some(&(interest_mask, ref interest)) = interests.get(i) {
                if interest_mask.contains(events) || events.intersects(EventMask::HUP | EventMask::ERROR) {
                    interest.fired.store(true, Ordering::Relaxed);
                    interest.task.notify();
                } else {
                    *remaining_interest_mask.get_or_insert(EventMask::empty()) |= interest_mask;
                    i += 1;
                    continue;
                }
            } else {
                break;
            }

            interests.swap_remove(i);
        }

        if let Some(mask) = remaining_interest_mask {
            registration.modify(mask, EventMode::EDGE | EventMode::ONE_SHOT).expect("failed to re-arm epoll registration");
        }
    }
}

impl queue::EventTag for EventTag {
    type EventQueue = Epoll;
}

impl queue::Handle for Handle {
    type EventQueue = Epoll;

    fn post_custom_event(&self, tag: <Self::EventQueue as EventQueue>::EventTag, data: usize) -> io::Result<()> {
        unimplemented!()
    }
}