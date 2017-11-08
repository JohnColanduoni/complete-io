pub mod net;

use ::evloop::{EventLoop as GenEventLoop, ConcurrentEventLoop, LocalHandle as GenLocalHandle, RemoteHandle as GenRemoteHandle};
use ::evloop::{Registrar, AsRegistrar};

use std::{io, mem};
use std::marker::PhantomData;
use std::cell::{UnsafeCell, RefCell};
use std::collections::{VecDeque, HashMap};
use std::sync::Arc;
use std::sync::atomic::{self, AtomicUsize, AtomicBool, Ordering};
use std::time::Duration;
use std::os::windows::prelude::*;

use futures::future;
use futures::prelude::*;
use futures::task::AtomicTask;
use futures::executor::{self, Spawn, Notify, NotifyHandle, UnsafeNotify};
use miow::Overlapped;
use miow::iocp::{CompletionPort as RawCompletionPort, CompletionStatus};
use winhandle::WinHandle;
use winapi::*;
use kernel32::*;
use ws2_32::*;

pub struct CompletionPort {
    inner: Arc<Inner>,
}

struct Inner {
    iocp: RawCompletionPort,
}

#[derive(Clone)]
pub struct LocalHandle {
    remote: RemoteHandle,
    _phantom: PhantomData<*mut ()>, // Prevent from being send
}

#[derive(Clone)]
pub struct RemoteHandle {
    inner: Arc<Inner>,
}
impl CompletionPort {
    pub fn new(max_concurrency: u32) -> io::Result<Self> {
        let inner = Arc::new(Inner {
            iocp: RawCompletionPort::new(max_concurrency)?,
        });

        Ok(CompletionPort { inner })
    }
}

impl GenEventLoop for CompletionPort {
    type LocalHandle = LocalHandle;
    type RemoteHandle = RemoteHandle;
    type Registrar = RemoteHandle;

    #[inline]
    fn handle(&self) -> Self::LocalHandle {
        LocalHandle {
            remote: self.remote(),
            _phantom: PhantomData,
        }
    }

    #[inline]
    fn remote(&self) -> Self::RemoteHandle {
        RemoteHandle {
            inner: self.inner.clone(),
        }
    }

    fn run<F>(&mut self, f: F) -> Result<F::Item, F::Error> where
        F: Future,
    {
        self.inner.run_future(f)
    }

    fn turn(&mut self, max_wait: Option<Duration>) {
        self.inner.poll_iocp(max_wait).expect("failed to poll IOCP");
    }
}

impl ConcurrentEventLoop for CompletionPort {
    fn run_concurrent<F>(&self, f: F) -> Result<F::Item, F::Error> where
        F: Future + Send,
    {
        // TODO: allow the primary future to be run by any thread in the IOCP maybe?
        self.inner.run_future(f)
    }

    fn turn_concurrent(&self, max_wait: Option<Duration>) {
        self.inner.poll_iocp(max_wait).expect("failed to poll IOCP");
    }

    fn run_local<F, R>(&self, f: F) -> Result<R::Item, R::Error> where
        F: FnOnce(&Self::LocalHandle) -> R,
        R: IntoFuture,
    {
        let handle = self.handle();
        self.inner.run_future(future::lazy(|| f(&handle)))
    }
}

impl GenLocalHandle for LocalHandle {
    type EventLoop = CompletionPort;

    fn remote(&self) -> &RemoteHandle {
        &self.remote
    }

    fn spawn<F>(&self, f: F) where
        F: Future<Item = (), Error = ()> + 'static
    {
        LocalSpawn::new(&self.remote.inner, Box::new(f));
    }

    fn spawn_fn<F, R>(&self, f: F) where
        F: FnOnce() -> R + 'static,
        R: IntoFuture<Item = (), Error = ()> + 'static,
    {
        self.spawn(future::lazy(f));
    }
}

impl AsRegistrar<CompletionPort> for LocalHandle {
    fn as_registrar(&self) -> &RemoteHandle {
        &self.remote
    }
}

impl RemoteHandle {
    /// Adds a WinSock socket to the IOCP.
    pub fn add_socket<S>(&self, s: &S) -> io::Result<()> where S: AsRawSocket {
        self.inner.iocp.add_socket(FUTURE_NOTIFY_IOCP_TOKEN, s)
    }
}

impl GenRemoteHandle for RemoteHandle {
    type EventLoop = CompletionPort;

    fn local(&self) -> Option<LocalHandle> {
        if self.inner.has_local_work_queue() {
            Some(LocalHandle {
                remote: self.clone(),
                _phantom: PhantomData,
            })
        } else {
            None
        }
    }

    fn spawn_locked<F, R>(&self, f: F) where
        F: FnOnce(&LocalHandle) -> R + Send + 'static,
        R: IntoFuture<Item = (), Error = ()>,
        R::Future: 'static,
    {
        let remote = self.clone();
        self.spawn(future::lazy(move || {
            let local = LocalHandle {
                remote,
                _phantom: PhantomData,
            };

            local.spawn(f(&local).into_future());

            Ok(())
        }))
    }

    fn spawn<F>(&self, f: F) where
        F: Future<Item = (), Error = ()> + Send + 'static,
    {
        let spawn: GlobalSpawn = executor::spawn(Box::new(f));
        Inner::spawn_remote(&self.inner, spawn).expect("failed to queue future on IOCP");
    }

    fn spawn_fn<F, R>(&self, f: F) where
        F: FnOnce(&LocalHandle) -> R + Send + 'static,
        R: IntoFuture<Item = (), Error = ()>,
        R::Future: Send + 'static,
    {
        let remote = self.clone();
        self.spawn(future::lazy(move || {
            let local = LocalHandle {
                remote,
                _phantom: PhantomData,
            };

            f(&local).into_future()
        }))
    }
}

impl Registrar for RemoteHandle {
    type EventLoop = CompletionPort;
}

impl AsRegistrar<CompletionPort> for RemoteHandle {
    fn as_registrar(&self) -> &RemoteHandle {
        self
    }
}

/// Allows overlapped IO operations to `notify` tasks.
/// 
/// This structure need only be used when adding bindings to additional objects capable
/// of overlapped IO. When passed to Windows APIs like `WSASend` or `ReadFile`, this
/// overlapped will ensure the given task is notified when the operation completes. Note
/// that you must ensure the IO object is added to the IOCP prior, or no notifications will
/// be received.
pub struct OverlappedTask(*const _OverlappedTask);

#[repr(C)] // The pointer for Overlapped and _OverlappedTask must be the same
struct _OverlappedTask {
    overlapped: Overlapped,
    task: AtomicTask,
    // TODO: consider padding to cache lines to prevent false sharing
    refcount: AtomicUsize,
}

unsafe impl Send for OverlappedTask {}
unsafe impl Sync for OverlappedTask {}

impl Drop for OverlappedTask {
    fn drop(&mut self) {
        if self.inner().refcount.fetch_sub(1, Ordering::Release) != 1 {
            return;
        }
        atomic::fence(Ordering::Acquire);

        unsafe { mem::drop(Box::from_raw(self.0 as *mut _OverlappedTask)) };
    }
}

impl Clone for OverlappedTask {
    fn clone(&self) -> Self {
        self.inner().refcount.fetch_add(1, Ordering::Relaxed);
        OverlappedTask(self.0)
    }
}

impl OverlappedTask {
    /// Creates an `OverlappedTask` that will notify the current task.
    pub fn new(_evloop: &RemoteHandle) -> OverlappedTask {
        let overlapped = Box::new(_OverlappedTask {
            overlapped: Overlapped::zero(),
            task: AtomicTask::new(),
            refcount: AtomicUsize::new(1),
        });
        overlapped.task.register();
        OverlappedTask(Box::into_raw(overlapped) as *const _OverlappedTask)
    }

    /// Alters the registered `Task` associated with this instance. This behaves like `AtomicTask::register`.
    pub fn register(&self) {
        self.inner().task.register();
    }

    /// Prepares the `OverlappedTask` for submission of an operation.
    /// 
    /// Users *must* ensure that the closure returns an error if and only if the operation failed to be
    /// submitted. Failing to do so may cause a use-after-free of the structure. The optimal way to use
    /// this function is to wrap precisely one system call that uses an `OVERLAPPED`.
    /// 
    /// This consumes the `OverlappedTask` because a reference to it will be placed in the IOCP when the
    /// operation is complete. Only one operation may be active for a given OverlappedTask at a time.
    #[inline]
    pub fn for_operation<F, R>(self, f: F) -> io::Result<R>
        where F: FnOnce(*mut OVERLAPPED) -> io::Result<R>
    {
        let overlapped = self.inner().overlapped.raw();
        match f(overlapped) {
            Ok(x) => {
                mem::forget(self); // Hand our +1 refcount to the IOCP
                Ok(x)
            },
            Err(err) => Err(err),
        }
    }

    pub fn poll_socket<S: AsRawSocket>(&self, socket: &S) -> Poll<(usize, DWORD), io::Error> {
        match unsafe { (*self.inner().overlapped.raw()).Internal as i32 } {
            STATUS_PENDING => Ok(Async::NotReady),
            _ => {
                let mut transferred: DWORD = 0;
                let mut flags: DWORD = 0;
                if unsafe { WSAGetOverlappedResult(
                    socket.as_raw_socket(),
                    self.inner().overlapped.raw(),
                    &mut transferred,
                    FALSE,
                    &mut flags,
                ) } == FALSE {
                    return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
                }

                Ok(Async::Ready((transferred as usize, flags)))
            },
        }
    }

    #[inline]
    fn inner(&self) -> &_OverlappedTask {
        unsafe { &*(self.0) }
    }
}

const SPAWN_REMOTE_IOCP_TOKEN: usize = 1;
const FUTURE_NOTIFY_IOCP_TOKEN: usize = 2;

type GlobalSpawn = Spawn<Box<Future<Item = (), Error = ()> + Send>>;

impl Inner {
    // Drives a spawn by posting it to the IOCP as a custom item. The item may be run by any thread that
    // enters the IOCP.
    fn spawn_remote(this: &Arc<Self>, spawn: GlobalSpawn) -> io::Result<()> {
        let mut overlapped = Box::new(OverlappedSpawn {
            overlapped: Overlapped::zero(),
            spawn: UnsafeCell::new(mem::ManuallyDrop::new(spawn)),
            event_loop: this.clone(),

            future_refcount: AtomicUsize::new(1),
            wrapper_refcount: AtomicUsize::new(1),
            signal_level: AtomicUsize::new(1),
        });

        let completion = CompletionStatus::new(0, SPAWN_REMOTE_IOCP_TOKEN, &mut overlapped.overlapped);
        Box::into_raw(overlapped); // Do not free OVERLAPPED; this will be done when dequeued from IOCP
        this.iocp.post(completion)?;

        Ok(())
    }

    fn run_future<F>(&self, f: F) -> Result<F::Item, F::Error> where
        F: Future,
    {
        let mut spawn = executor::spawn(f);
        let notify = Arc::new(SimpleNotify {
            notified: AtomicBool::new(true),
        });

        loop {
            if notify.notified.swap(false, Ordering::SeqCst) { // TODO: relax ordering?
                match spawn.poll_future_notify(&notify, 0) {
                    Ok(Async::Ready(t)) => return Ok(t),
                    Ok(Async::NotReady) => {},
                    Err(err) => return Err(err),
                }
            }
            self.poll_iocp(None).expect("failed to poll IOCP");
        }
    }

    fn poll_iocp(&self, timeout: Option<Duration>) -> io::Result<()> {
        // Clear local work queue
        while let Some(spawn) = self.with_local_work_queue(|queue| queue.pop_front()) {
            spawn.poll();
        }

        // TODO: maybe dequeue multiple events?
        let mut completion_buffer: [CompletionStatus; 1];
        let completions;
        unsafe {
            completion_buffer = mem::zeroed();
            let mut entries_removed: DWORD = 0;
            let ms_timeout = if let Some(timeout) = timeout {
                (timeout.as_secs() * 1000) as u32 + timeout.subsec_nanos() / 1_000_000
            } else {
                INFINITE
            };
            if GetQueuedCompletionStatusEx(
                self.iocp.as_raw_handle(),
                completion_buffer.as_mut_ptr() as *mut _,
                completion_buffer.len() as ULONG,
                &mut entries_removed,
                ms_timeout,
                TRUE, // Perform an alertable wait so we receive notifications to thread-local tasks
            ) == 0 {
                let error = GetLastError();
                match error {
                    // Timeout is okay
                    WAIT_TIMEOUT if timeout.is_some() => {
                        return Ok(());
                    },
                    // Local tasks are queued as APCs, which will cause the call to be interrupted with WAIT_IO_COMPLETION
                    WAIT_IO_COMPLETION => {
                        // Clear local work queue
                        while let Some(spawn) = self.with_local_work_queue(|queue| queue.pop_front()) {
                            spawn.poll();
                        }

                        return Ok(());
                    },
                    error => {
                        return Err(io::Error::from_raw_os_error(error as i32));
                    }
                }
            }
            completions = &mut completion_buffer[0..entries_removed as usize];
        }

        for completion in completions.iter() {
            match completion.token() {
                SPAWN_REMOTE_IOCP_TOKEN => unsafe {
                    let overlapped_spawn = &*(completion.overlapped() as *const OverlappedSpawn);
                    let pre_notify = mem::ManuallyDrop::new(OverlappedSpawnIntoNotify(overlapped_spawn));

                    let mut init_signal_level = overlapped_spawn.signal_level.load(Ordering::SeqCst); // TODO: weaken?
                    loop {
                        match (*overlapped_spawn.spawn.get()).poll_future_notify(&*pre_notify, 0) {
                            Ok(Async::Ready(())) => {
                                // Release reference to task
                                overlapped_spawn.drop_future_ref();
                                break;
                            },
                            Ok(Async::NotReady) => {
                                // The future is waiting for something
                                // We must make sure that no notifications were received while we were running Poll. If there were,
                                // we poll again.
                                // TODO: maybe try to re-poll a certain amount of times, and then put in queue to prevent blocking this
                                // thread?
                                // TODO: relax ordering?
                                match overlapped_spawn.signal_level.compare_exchange(init_signal_level, 0, Ordering::SeqCst, Ordering::SeqCst) {
                                    Ok(_) => break,
                                    Err(level) => {
                                        // Update notify count and retry
                                        init_signal_level = level;
                                        continue;
                                    },
                                }
                            },
                            Err(()) => {
                                // Release reference to task
                                overlapped_spawn.drop_future_ref();
                                panic!("raw future in event loop resolved to an error");
                            },
                        }
                    }
                },
                FUTURE_NOTIFY_IOCP_TOKEN => {
                    // TODO: if notify acts on an OverlappedSpawn, move directly to polling that future. Otherwise every IO call
                    // bounces off the IOCP twice.
                    let overlapped_task = OverlappedTask(completion.overlapped() as *const _OverlappedTask);
                    overlapped_task.inner().task.notify();
                },
                _ => panic!("unexpected IOCP token"),
            }
        }

        Ok(())
    }

    fn with_local_work_queue<F, R>(&self, f: F) -> R where
        F: FnOnce(&mut VecDeque<LocalSpawn>) -> R,
    {
        LOCAL_WORK_QUEUE.with(|hashmap| {
            let mut hashmap = hashmap.borrow_mut();
            let queue = hashmap
                .entry(self as *const Inner as usize)
                .or_insert_with(|| Default::default());
            f(queue)
        })
    }

    fn has_local_work_queue(&self) -> bool {
        LOCAL_WORK_QUEUE.with(|hashmap| {
            hashmap.borrow().contains_key(&(self as *const Inner as usize))
        })
    }
}

// The life and times of an OverlappedSpawn deserves some mention. This
// structure is the center of driving a Task which is not locked to a
// particular IOCP thread.
//
// The task is always in one of the following states over its lifetime:
//
// * Suspened - The task is either waiting for a notification or has just been
//              created but not yet placed in the IOCP.
// * Queued   - The task is referenced by a CompletionStatus and will eventually
//              be removed from the IOCP by a worker.
// * Polling  - The task was removed from the IOCP and its poll is now running on
//              the worker thread which removed it.
// * Completed- The task has completed.
//
// The most complicated part of this lifecycle is notifications. Since a task may
// subscribe to multiple notifications (and possible experience spruious wakeups)
// they may happen at any one of these stages. If suspended, the task must be
// placed onto the IOCP *exactly once* no matter how many notifications occur while 
// it is suspended (otherwise it may be dequeued by multiple workers and polled 
// concurrently, which is illegal). Notifications while queued must be handled in
// the same way. Notifications while polling must cause the task to be polled again,
// since the poll may have already passed over the state that the notification indicates
// changed during the current iteration. Finally notifications while in the complete
// state can be safely ignored, but the notification handles themselves may still be
// present, preventing the overall structure from being dropped. An extra complication
// for the completed state is the fact that the contained future is allowed to not be
// of static type (e.g. if it was added by EventLoop::run_concurrent) so it *must* be
// dropped before that stack frame is popped, while the notifications may still be live.
//
// There are a series of (wait-free) synchronization mechanisms employed to ensure proper
// operation. First, we effectively have an Arc with weak pointers (we don't use the real
// variant because of layout issues). The notification handles are weak, and there is only
// at most one strong pointer that coincides with a pre-completed state. The task itself
// is always dropped when the future is completed, but the structure as a whole will only
// be dropped when all the notification handles are gone as well. To ensure proper
// notification delivery, an atomic variable controls a sort of meta "signal level" state.
// It is set to zero when suspended, and 1+ for queued or polling. Notifications always fetch_add
// the signal level. If it was previously zero and the task is not completed, the task is put 
// onto the IOCP by the notifier. Otherwise the notififier takes no other action. This takes care of the suspended
// and queued states. For the polling case, the worker loads the signal level at the start
// of the poll iteration. At the end it does a CAS, setting the value back to zero iff it
// has not changed. This will be the case iff no notifications were delivered while polling.
// Otherwise it repeats the same operation, ensuring that no notifications are lost. If a poll
// leads to the completion of the future the signal level is not reset, causing late notifiers
// to take no action (and therefore *not* put the task back on the IOCP). Otherwise it is set
// to zero to enter the suspended state and await the first notification.
#[repr(C)] // Overlapped needs to have the same pointer as the overall structure
struct OverlappedSpawn {
    overlapped: Overlapped,
    spawn: UnsafeCell<mem::ManuallyDrop<GlobalSpawn>>,
    event_loop: Arc<Inner>,
    // TODO: consider padding to cache lines to prevent false sharing
    future_refcount: AtomicUsize,
    wrapper_refcount: AtomicUsize, // The referrers of the future are counted as a single +1 wrapper refcount
    signal_level: AtomicUsize,
}

unsafe impl Send for OverlappedSpawn { }
unsafe impl Sync for OverlappedSpawn { }

impl OverlappedSpawn {
    #[inline]
    unsafe fn add_wrapper_ref(&self) {
        self.wrapper_refcount.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    unsafe fn drop_wrapper_ref(&self) {
        // Only use an acquire fence if we are going to drop the struct
        if self.wrapper_refcount.fetch_sub(1, Ordering::Release) != 1 {
            return;
        }
        atomic::fence(Ordering::Acquire);
        mem::drop(Box::from_raw(self as *const OverlappedSpawn as *mut OverlappedSpawn));
    }

    unsafe fn drop_future_ref(&self) {
        // Only use an acquire fence if we are going to drop the struct
        if self.future_refcount.fetch_sub(1, Ordering::Release) != 1 {
            return;
        }
        atomic::fence(Ordering::Acquire);

        mem::ManuallyDrop::drop(&mut *self.spawn.get());

        // The last strong reference holds a reference to the wrapper
        self.drop_wrapper_ref();
    }
}

impl Notify for OverlappedSpawn {
    fn notify(&self, _id: usize) {
        // Signal level is 0 iff we are suspended. In that case it's our job to put the task back on the IOCP.
        if self.signal_level.fetch_add(1, Ordering::SeqCst) == 0 { // TODO: weaken ordering?
            let completion = CompletionStatus::new(0, SPAWN_REMOTE_IOCP_TOKEN, &self.overlapped as *const Overlapped as *mut Overlapped);
            self.event_loop.iocp.post(completion).expect("failed to post completion status to IOCP");
        }
    }
}

unsafe impl UnsafeNotify for OverlappedSpawn {
    unsafe fn clone_raw(&self) -> NotifyHandle {
        self.add_wrapper_ref();
        NotifyHandle::new(self as *const Self as *mut Self)
    }

    unsafe fn drop_raw(&self) {
        self.drop_wrapper_ref();
    }
}

struct OverlappedSpawnIntoNotify(*const OverlappedSpawn);

impl Clone for OverlappedSpawnIntoNotify {
    fn clone(&self) -> Self {
        unsafe {
            (*self.0).add_wrapper_ref();
            OverlappedSpawnIntoNotify(self.0)
        }
    }
}

impl Drop for OverlappedSpawnIntoNotify {
    fn drop(&mut self) {
        unsafe { (*self.0).drop_wrapper_ref() }
    }
}

impl Into<NotifyHandle> for OverlappedSpawnIntoNotify {
    fn into(self) -> NotifyHandle {
        // OverlappedSpawnIntoNotify already has a wrapper refcount if owned
        unsafe { NotifyHandle::new(self.0 as *mut OverlappedSpawn) }
    }
}

struct SimpleNotify {
    notified: AtomicBool,
}

impl Notify for SimpleNotify {
    fn notify(&self, _id: usize) {
        self.notified.store(true, Ordering::SeqCst); // TODO: relax ordering?
    }
}


thread_local! {
    static LOCAL_WORK_QUEUE: RefCell<HashMap<usize, VecDeque<LocalSpawn>>> = Default::default();
}

// Tasks that are locked to a single thread must be handled differently from those that can migrate.
// We can't put them on the IOCP, but we can use a less well known future called user APCs. This
// allows us to queue arbitrary callbacks from any thread that will only be run on the given thread
// during an "alertable wait". Conveniently enough GetQueuedCompletionStatusEx can perform such a
// wait, and the APC will cause it to wake up.
//
// All notifications will have an Arc<_LocalSpawn>, but there should only ever be one LocalSpawn.
pub struct LocalSpawn(Arc<_LocalSpawn>);

struct _LocalSpawn {
    thread: WinHandle,
    evloop: Arc<Inner>,
    future: UnsafeCell<Spawn<Box<Future<Item = (), Error = ()>>>>,
    signal_level: AtomicUsize,
}

unsafe impl Send for _LocalSpawn {}
unsafe impl Sync for _LocalSpawn {}

impl LocalSpawn {
    fn new(evloop: &Arc<Inner>, future: Box<Future<Item = (), Error = ()>>) {
        let local_spawn = LocalSpawn(Arc::new(_LocalSpawn {
            thread: unsafe { WinHandle::from_raw_unchecked(GetCurrentThread()) },
            evloop: evloop.clone(),
            future: UnsafeCell::new(executor::spawn(future)),
            signal_level: AtomicUsize::new(1), // signal level is 1 because we will be placing on the queue immediately
        }));
        evloop.with_local_work_queue(|queue| queue.push_back(local_spawn));
    }

    fn poll(self) {
        let mut init_signal_level = self.0.signal_level.load(Ordering::SeqCst); // TODO: relax ordering?
        loop {
            let future = unsafe { &mut *self.0.future.get() };
            match future.poll_future_notify(&self.0, 0) {
                Ok(Async::Ready(())) => {
                    break;
                },
                Ok(Async::NotReady) => {
                    // The future is waiting for something
                    // We must make sure that no notifications were received while we were running Poll. If there were,
                    // we poll again.
                    // TODO: maybe try to re-poll a certain amount of times, and then put in queue to prevent blocking this
                    // thread?
                    // TODO: relax ordering?
                    match self.0.signal_level.compare_exchange(init_signal_level, 0, Ordering::SeqCst, Ordering::SeqCst) {
                        Ok(_) => break,
                        Err(level) => {
                            // Update notify count and retry
                            init_signal_level = level;
                            continue;
                        }
                    }
                },
                Err(()) => {
                    panic!("raw future in event loop resolved to an error");
                },
            }
        }
    }
}

impl Notify for _LocalSpawn {
    fn notify(&self, _id: usize) {
        if self.signal_level.fetch_add(1, Ordering::SeqCst) == 0 { // TODO: relax ordering?
            unsafe {
                // We need to send an Arc to the APC, but we don't want to drop ourselves. I don't love this :/
                let self_arc = Arc::from_raw(self as *const _LocalSpawn);
                let copy_arc = self_arc.clone();
                mem::forget(self_arc);

                if QueueUserAPC(
                    Some(notify_local_work),
                    self.thread.get(),
                    Arc::into_raw(copy_arc) as _,
                ) == 0 {
                    let last_err = GetLastError();
                    // ERROR_GEN_FAILURE is returned if the thread has terminated
                    if last_err != ERROR_GEN_FAILURE {
                        panic!("QueueUserAPC failed: {:?}", io::Error::from_raw_os_error(last_err as i32));
                    }
                }
            }
        }
    }
}

unsafe extern "system" fn notify_local_work(param: ULONG_PTR) {
    let local_spawn = Arc::from_raw(param as *const _LocalSpawn);
    local_spawn.evloop.with_local_work_queue(|queue| {
        queue.push_back(LocalSpawn(local_spawn.clone())) // TODO: get rid of extra clone somehow
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::thread;

    use futures::{self, future};

    #[test]
    fn single_thread_remote_spawn() {
        let iocp = CompletionPort::new(1).unwrap();
        let x = Arc::new(AtomicUsize::new(0));
        iocp.remote().spawn(future::lazy({
            let x = x.clone();
            move || {
                x.store(1, Ordering::SeqCst);
                Ok(())
            }
        }));
        iocp.turn_concurrent(None);
        assert_eq!(1, x.load(Ordering::SeqCst));
    }

    #[test]
    fn multithread_remote_spawn() {
        let iocp = CompletionPort::new(1).unwrap();
        let x = Arc::new(AtomicUsize::new(0));
        iocp.remote().spawn(future::lazy({
            let x = x.clone();
            move || {
                x.store(1, Ordering::SeqCst);
                Ok(())
            }
        }));
        thread::spawn(move || {
            iocp.turn_concurrent(None);
        }).join().unwrap();
        assert_eq!(1, x.load(Ordering::SeqCst));
    }

    #[test]
    fn single_thread_local_spawn() {
        let iocp = CompletionPort::new(1).unwrap();
        let x = Arc::new(AtomicUsize::new(0));
        iocp.handle().spawn(future::lazy({
            let x = x.clone();
            move || {
                x.store(1, Ordering::SeqCst);
                Ok(())
            }
        }));
        iocp.turn_concurrent(Some(Duration::from_millis(1)));
        assert_eq!(1, x.load(Ordering::SeqCst));
    }

    #[test]
    fn single_thread_local_notify() {
        let iocp = CompletionPort::new(1).unwrap();
        let x = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = futures::sync::oneshot::channel();
        iocp.handle().spawn(future::lazy({
            let x = x.clone();
            move || {
                rx.map(move |value| {
                    x.store(value, Ordering::SeqCst);
                }).map_err(|err| panic!("{:?}", err))
            }
        }));
        iocp.turn_concurrent(Some(Duration::from_millis(1)));
        tx.send(1).unwrap();
        iocp.turn_concurrent(None);
        assert_eq!(1, x.load(Ordering::SeqCst));
    }
}