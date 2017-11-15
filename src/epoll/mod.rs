pub mod net;

use ::queue::{self, EventQueue, CustomEventHandler};
use ::io::{EventRegistrar, AsRegistrar};

use std::{io, mem, ptr};
use std::cell::UnsafeCell;
use std::sync::{Arc, Weak, Mutex, Once, ONCE_INIT};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use std::os::raw::{c_int, c_void};
use std::os::unix::prelude::*;

use futures::prelude::*;
use futures::task::AtomicTask;
use crossbeam::sync::SegQueue;
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
pub struct EventTag {
    ptr: *const _CustomEvent,
    epoll_fd: RawFd,
}

#[derive(Clone)]
pub struct Interrupt(Arc<_Interrupt>);

struct _Interrupt {
    thread_id: AtomicUsize,
}

unsafe impl Send for EventTag {}
unsafe impl Sync for EventTag {}

struct Inner {
    fd: RawFd,
    custom_events: Mutex<Vec<Box<_CustomEvent>>>,
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
            custom_events: Default::default(),
        })})
    }
}

impl EventQueue for Epoll {
    type Handle = Handle;
    type EventTag = EventTag;
    type Interrupt = Interrupt;

    fn handle(&self) -> Handle { Handle { inner: self.inner.clone(), } }

    fn turn(&self, max_wait: Option<Duration>) -> io::Result<usize> {
        self.inner.poll(max_wait, None)
    }

    fn turn_interruptible(&self, max_wait: Option<Duration>, interrupt: &Interrupt) -> io::Result<usize> {
        self.inner.poll(max_wait, Some(interrupt))
    }

    fn new_custom_event(&self, handler: CustomEventHandler<Self>) -> io::Result<Self::EventTag> {
        let event_fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK | libc::EFD_SEMAPHORE) };
        if event_fd == -1 {
            let error = io::Error::last_os_error();
            error!("eventfd failed: {}", error);
            return Err(error);
        }

        let reg_handler = Arc::new(CustomEventRegistrationHandler {
            event: UnsafeCell::new(ptr::null()),
        });
        let registration = unsafe { self.handle().register_fd_raw(event_fd, EventMask::READ, EventMode::ONE_SHOT, reg_handler.clone())? };

        let event = Box::new(_CustomEvent {
            handler,
            event_fd,
            registration,
            queue: SegQueue::new(),
        });
        unsafe { *reg_handler.event.get() = &*event; }
        let tag = EventTag {
            ptr: &*event,
            epoll_fd: self.inner.fd,
        };

        {
            let mut custom_events = self.inner.custom_events.lock().unwrap();
            custom_events.push(event);
        }

        Ok(tag)
    }
}

impl EventRegistrar for Epoll {
    type RegHandle = Handle;
}

impl Handle {
    pub fn register_fd<F: AsRawFd + ?Sized>(&self, fd: &F, event_mask: EventMask, mode: EventMode, handler: Arc<RegistrationHandler>) -> io::Result<Registration> {
        unsafe { self.register_fd_raw(fd.as_raw_fd(), event_mask, mode, handler) }
    }

    pub unsafe fn register_fd_raw(&self, fd: RawFd, event_mask: EventMask, mode: EventMode, handler: Arc<RegistrationHandler>) -> io::Result<Registration> {
        let registration = Registration(Arc::new(_Registration {
            fd,
            queue: self.inner.clone(),
            epoll_data: 0.into(),
            handler,
        }));
        let mut event = libc::epoll_event {
            events: event_mask.bits() | mode.bits(),
            u64: mem::transmute::<Weak<_Registration>, usize>(Arc::downgrade(&registration.0)) as u64,
        };
        *registration.0.epoll_data.get() = event.u64;
        if libc::epoll_ctl(self.inner.fd, libc::EPOLL_CTL_ADD, fd, &mut event) == -1 {
            // Don't EPOLL_CTL_DEL
            mem::drop(mem::transmute::<_, Weak<_Registration>>(event.u64));
            mem::forget(Arc::try_unwrap(registration.0).unwrap_or_else(|_| unreachable!()));
            let error = io::Error::last_os_error();
            error!("adding via epoll_ctl failed: {}", error);
            return Err(error);
        } else {
            Ok(registration)
        }
    }
}

impl AsRegistrar<Epoll> for Handle {
    fn as_registrar(&self) -> &Self { self }
}

impl queue::Interrupt for Interrupt {
    type EventQueue = Epoll;

    fn new() -> Self {
        Interrupt(Arc::new(_Interrupt {
            thread_id: AtomicUsize::new(0),
        }))
    }

    fn interrupt(&self) -> io::Result<bool> {
        match self.0.thread_id.load(Ordering::SeqCst) { // TODO: weaken?
            0 => Ok(false),
            thread_id => {
                match unsafe { libc::pthread_kill(thread_id as libc::pthread_t, libc::SIGUSR2) } {
                    0 => Ok(true),
                    err => Err(io::Error::from_raw_os_error(err)),
                }
            },
        }
    }
}

static mut OLD_SIGUSR2_HANDLER: Option<libc::sigaction> = None;

static SIGUSR2_INSTALL: Once = ONCE_INIT;

unsafe extern "C" fn interrupt_sig_handler(signum: c_int, siginfo: *mut libc::siginfo_t, context: *mut c_void) {
    if let Some(ref old_handler) = OLD_SIGUSR2_HANDLER {
        if old_handler.sa_sigaction == libc::SIG_IGN || old_handler.sa_sigaction == libc::SIG_DFL {
            return;
        }

        if old_handler.sa_flags & libc::SA_SIGINFO != 0 {
            let fptr: unsafe extern "C" fn(c_int, *mut libc::siginfo_t, *mut c_void) = mem::transmute(old_handler.sa_sigaction);
            fptr(signum, siginfo, context);
        } else {
            let fptr: unsafe extern "C" fn(c_int) = mem::transmute(old_handler.sa_sigaction);
            fptr(signum);
        }
    }
}

impl Inner {
    fn poll(&self, max_wait: Option<Duration>, interrupt: Option<&Interrupt>) -> io::Result<usize> {
        let timeout_ms = if let Some(timeout) = max_wait {
            (timeout.as_secs() as u32 * 1000 + (timeout.subsec_nanos() / 1_000_000)) as i32
        } else {
            -1
        };

        loop {
            unsafe {
                let mut events: [libc::epoll_event; EVENT_BUFFER_SIZE] = mem::zeroed();
                let event_count = if let Some(interrupt) = interrupt {
                    // Install custom SIGUSR2 handler
                    SIGUSR2_INSTALL.call_once(|| {
                        let mut new_sigaction: libc::sigaction = mem::zeroed();
                        let mut old_sigaction: libc::sigaction = mem::zeroed();
                        new_sigaction.sa_sigaction = interrupt_sig_handler as usize;
                        new_sigaction.sa_flags = libc::SA_SIGINFO;
                        if libc::sigaction(libc::SIGUSR2, &new_sigaction, &mut old_sigaction) == -1 {
                            panic!("sigaction failed to install SIGUSR2 handler");
                        }
                        OLD_SIGUSR2_HANDLER = Some(old_sigaction);
                    });

                    // Add our thread handle to interrupt
                    interrupt.0.thread_id.store(libc::pthread_self() as usize, Ordering::SeqCst); // TODO: weaken?

                    // Poll, only allowing SIGUSR2 to interrupt us
                    let mut sig_set: libc::sigset_t = mem::zeroed();
                    libc::sigfillset(&mut sig_set);
                    libc::sigdelset(&mut sig_set, libc::SIGUSR2);
                    let result = match libc::epoll_pwait(self.fd, events.as_mut_ptr(), events.len() as _, timeout_ms, &mut sig_set) {
                        -1 => Err(io::Error::last_os_error()),
                        n => Ok(n as usize),
                    };

                    // Remove our thread handle from interrupt
                    interrupt.0.thread_id.store(0, Ordering::SeqCst); // TODO: weaken?
                    match result {
                        Err(ref err) if err.kind() == io::ErrorKind::Interrupted => {
                            // We were interrupted via Interrupt
                            return Ok(0);
                        },
                        Err(error) => {
                            error!("epoll_wait failed: {}", error);
                            return Err(error);
                        },
                        Ok(0) => return Ok(0),
                        Ok(count) => count,
                    }
                } else {
                    match libc::epoll_wait(self.fd, events.as_mut_ptr(), events.len() as _, timeout_ms) {
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
                    }
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
        if tag.epoll_fd != self.inner.fd {
            panic!("the given EventTag is not from this Epoll instance");
        }

        let custom_event = unsafe { &*tag.ptr };

        custom_event.queue.push(data);
        
        if unsafe { libc::write(custom_event.event_fd, &1u64 as *const u64 as _, 8) } == -1 {
            let error = io::Error::last_os_error();
            error!("write to signalfd for custom event failed: {}", error);
            return Err(error);
        }

        Ok(())
    }
}

struct _CustomEvent {
    handler: CustomEventHandler<Epoll>,
    event_fd: RawFd,
    registration: Registration,
    queue: SegQueue<usize>,
}

impl Drop for _CustomEvent {
    fn drop(&mut self) {
        unsafe { libc::close(self.event_fd); }
    }
}

struct CustomEventRegistrationHandler {
    event: UnsafeCell<*const _CustomEvent>,
}

impl RegistrationHandler for CustomEventRegistrationHandler {
    fn handle_event(&self, registration: &Registration, _events: EventMask) {
        let custom_event = unsafe { &**self.event.get() };

        let mut count: u64 = 0;
        if unsafe { libc::read(custom_event.event_fd, &mut count as *mut u64 as _, 8) } == -1 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                count = 0;
            } else {
                panic!("read of eventfd failed: {}", error);
            }
        }

        if count != 0 {
            let event_tag = EventTag {
                ptr: custom_event,
                epoll_fd: custom_event.registration.0.queue.fd,
            };
            while let Some(data) = custom_event.queue.try_pop() {
                (custom_event.handler)(event_tag, data);
            }
        }

        registration.modify(EventMask::READ, EventMode::ONE_SHOT).expect("failed to re-arm eventfd for custom event");
    }
}

#[cfg(test)]
mod tests {
    make_evqueue_tests!(::epoll::Epoll::new().unwrap());
}