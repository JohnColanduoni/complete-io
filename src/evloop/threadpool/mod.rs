use ::queue::{EventQueue, Handle as EqHandle};
use ::io::{AsRegistrar};
use ::evloop::{Handle as GenHandle};
use ::net::{NetEventQueue, NetEventLoop};

use std::{io, thread, panic, mem, ptr};
use std::cell::{UnsafeCell, Cell, RefCell};
use std::collections::VecDeque;
use std::sync::{Arc, RwLock};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use futures;
use futures::prelude::*;
use futures::future;
use futures::executor::{self, Spawn, Notify};
use coco;
use coco::deque::Steal;
use crossbeam::sync::AtomicOption;
use num_cpus;

pub struct ThreadPool<Q: EventQueue> {
    inner: Arc<_ThreadPool<Q>>,
}

impl<Q: EventQueue> Drop for ThreadPool<Q> {
    fn drop(&mut self) {
        self.begin_shutdown();
    }
}

pub struct Handle<Q: EventQueue> {
    pool: Arc<_ThreadPool<Q>>,
    event_queue_handle: Q::Handle,
}

pub struct LocalHandle<Q: EventQueue> {
    global: Handle<Q>,
    worker_self: *const WorkerSelf<Q>,
}

struct _ThreadPool<Q: EventQueue> {
    pub event_queue: Arc<Q>,
    pub submit_remote_work_event: UnsafeCell<Q::EventTag>,
    pub active: AtomicBool,
    pub workers: RwLock<Vec<Arc<_Worker<Q>>>>,
}

unsafe impl<Q: EventQueue> Send for _ThreadPool<Q> {}
unsafe impl<Q: EventQueue> Sync for _ThreadPool<Q> {}

struct _Worker<Q: EventQueue> {
    pub pool: Arc<_ThreadPool<Q>>,
    pub work_queue: coco::deque::Stealer<GlobalTask>,
    pub join_handle: AtomicOption<thread::JoinHandle<()>>,
}

impl<Q: EventQueue> ThreadPool<Q> {
    pub fn new(event_queue: Q) -> io::Result<Self> {
        Self::with_concurrency(event_queue, num_cpus::get())
    }

    pub fn with_concurrency(event_queue: Q, concurrency: usize) -> io::Result<Self> {
        let inner = Arc::new(_ThreadPool::<Q> {
            event_queue: Arc::new(event_queue),
            submit_remote_work_event: unsafe { mem::uninitialized() },
            active: AtomicBool::new(true),
            workers: Default::default(),
        });
        let submit_remote_work_event = inner.event_queue.new_custom_event(Box::new({
            let inner = inner.clone();
            move |_event_tag, data| handle_remote_spawn(&inner, data)
        }))?;
        unsafe { ptr::write(inner.submit_remote_work_event.get(), submit_remote_work_event); }

        {
            let mut workers = inner.workers.write().unwrap();
            for _ in 0..concurrency {
                workers.push(run_worker(inner.clone(), inner.event_queue.clone()));
            }
        }

        Ok(ThreadPool { inner })
    }

    // Initiates the shutdown process, but does not wait for all workers to exit
    pub fn begin_shutdown(&self) {
        self.inner.active.store(false, Ordering::Relaxed);
    }

    // Stops the workers and waits for them to exit
    pub fn stop(self) {
        self.begin_shutdown();

        unimplemented!()
    }
}

impl<Q: EventQueue> super::EventLoop for ThreadPool<Q> {
    type Handle = Handle<Q>;
    type LocalHandle = LocalHandle<Q>;
    type EventRegistrar = Q;

    fn handle(&self) -> Handle<Q> { Handle { 
        pool: self.inner.clone(),
        event_queue_handle: self.inner.event_queue.handle(),
    } }

    fn local_handle(&self) -> Option<LocalHandle<Q>> {
        unsafe { 
            if let Some(worker_self) = current_worker_handle(&self.inner) {
                Some(LocalHandle {
                    global: self.handle(),
                    worker_self,
                })
            } else {
                None
            }
        }
    }

    fn run_future<F: Future>(&mut self, _f: F) -> Result<F::Item, F::Error> {
        unimplemented!()
    }

    fn run<F, R>(&mut self, f: F) -> Result<R::Item, R::Error> where
        F: FnOnce(&Self::LocalHandle) -> R + Send + 'static,
        R: IntoFuture,
        R::Future: Send + 'static,
        R::Item: Send + 'static, R::Error: Send + 'static,
    {
        // TODO: add current thread as temporary worker
        let (tx, rx) = futures::sync::oneshot::channel();
        self.handle().spawn(move |handle| {
            f(handle).into_future().then(move |x| {
                let _ = tx.send(x);
                Ok(())
            })
        });
        rx.wait().expect("channel for receiving future dropped, likely due to panic in spawned future")
    }
}

impl<Q: EventQueue> super::ConcurrentEventLoop for ThreadPool<Q> {
}

impl<Q: EventQueue> super::Handle for Handle<Q> {
    type EventLoop = ThreadPool<Q>;

    fn local(&self) -> Option<LocalHandle<Q>> {
        unsafe { 
            if let Some(worker_self) = current_worker_handle(&self.pool) {
                Some(LocalHandle {
                    global: self.clone(),
                    worker_self,
                })
            } else {
                None
            }
        }
    }

    fn spawn<F, R>(&self, f: F) where
        F: FnOnce(&LocalHandle<Q>) -> R + Send + 'static,
        R: IntoFuture<Item = (), Error = ()>,
        R::Future: Send + 'static,
    {
        let handle = self.clone();
        self.spawn_future(future::lazy(move || {
            let handle = handle.local().expect("task run on invalid thread");
            f(&handle).into_future()
        }));
    }

    /// Spawns a future on a random worker of the `EventLoop` and locks it on that thread.
    fn spawn_locked<F, R>(&self, _f: F) where
        F: FnOnce(&LocalHandle<Q>) -> R + Send + 'static,
        R: IntoFuture<Item = (), Error = ()>,
        R::Future: 'static,
    {
        unimplemented!()
    }

    fn spawn_future<F>(&self, f: F) where
        F: Future<Item = (), Error = ()> + Send + 'static,
    {
        let task = Arc::new(_GlobalTask {
            spawn: UnsafeCell::new(executor::spawn(Box::new(f))),
            signal_level: AtomicUsize::new(1),
        });
        self.pool.event_queue.handle().post_custom_event(unsafe { *self.pool.submit_remote_work_event.get()}, Arc::into_raw(task) as usize)
            .expect("failed to post work via custom event to EventQueue");
    }
}

impl<Q: EventQueue> super::LocalHandle for LocalHandle<Q> {
    type EventLoop = ThreadPool<Q>;

    /// Gets the underlying global `EventLoop` handle which is not locked tp a specific thread
    fn global(&self) -> &Handle<Q> { &self.global }

    fn spawn_local<F, R>(&self, f: F) where
        F: FnOnce(&Self) -> R + 'static,
        R: IntoFuture<Item = (), Error = ()>,
        R::Future: 'static,
    {
        let handle = self.clone();
        self.spawn_local_future(future::lazy(move || {
            f(&handle).into_future()
        }));
    }

    fn spawn_local_future<F>(&self, f: F) where
        F: Future<Item = (), Error = ()> + 'static,
    {
        let worker_self = unsafe { &*self.worker_self };
        let task = LocalTask(Arc::new(_LocalTask {
            spawn: UnsafeCell::new(executor::spawn(Box::new(f))),
            signal_level: AtomicUsize::new(1),
        }));
        worker_self.local_work_queue.borrow_mut().push_front(task);
    }
}

struct WorkerSelf<Q: EventQueue> {
    worker: Arc<_Worker<Q>>,
    work_queue: coco::deque::Worker<GlobalTask>,
    local_work_queue: RefCell<VecDeque<LocalTask>>,
}

trait GenWorkerSelf {
    fn work_queue(&self) -> &coco::deque::Worker<GlobalTask>;
    fn thread_pool_ptr(&self) -> usize;
}

impl<Q: EventQueue> GenWorkerSelf for WorkerSelf<Q> {
    #[inline]
    fn work_queue(&self) -> &coco::deque::Worker<GlobalTask> {
        &self.work_queue
    }

    fn thread_pool_ptr(&self) -> usize {
        &*self.worker.pool as *const _ThreadPool<Q> as usize
    }
}

#[inline]
unsafe fn current_worker_handle<'a, Q: EventQueue>(pool_ref: &_ThreadPool<Q>) -> Option<*const WorkerSelf<Q>> {
    if let Some(current_worker) = CURRENT_WORKER.with(|x| x.get()) {
        if (*current_worker).thread_pool_ptr() == &*pool_ref as *const _ThreadPool<Q> as usize {
            Some(current_worker as *const WorkerSelf<Q>)
        } else {
            None
        }
    } else {
        None
    }
}

thread_local! {
    static CURRENT_WORKER: Cell<Option<*const GenWorkerSelf>> = Default::default();
}

struct LocalTask(Arc<_LocalTask>);

struct _LocalTask {
    spawn: UnsafeCell<Spawn<Box<Future<Item = (), Error = ()>>>>,
    // Signal level is 0 when suspended. Notify increments it
    signal_level: AtomicUsize,
}

unsafe impl Send for _LocalTask {}
unsafe impl Sync for _LocalTask {}

struct GlobalTask(Arc<_GlobalTask>);

struct _GlobalTask {
    spawn: UnsafeCell<Spawn<Box<Future<Item = (), Error = ()> + Send>>>,
    // Signal level is 0 when suspended. Notify increments it
    signal_level: AtomicUsize,
}

unsafe impl Send for _GlobalTask {}
unsafe impl Sync for _GlobalTask {}

impl GlobalTask {
    fn do_poll(self) {
        let mut init_signal_level = self.0.signal_level.load(Ordering::SeqCst); // TODO: weaken?
        loop {
            match unsafe { (*self.0.spawn.get()).poll_future_notify(&self.0, 0) } {
                Ok(Async::NotReady) => {
                    match self.0.signal_level.compare_exchange(init_signal_level, 0, Ordering::SeqCst, Ordering::SeqCst) { // TODO: weaken?
                        Ok(_) => break,
                        Err(level) => {
                            init_signal_level = level;
                            continue;
                        },
                    }
                },
                // TODO: drop future inside _GlobalTask here, since notify handles may persist
                Ok(Async::Ready(())) => break,
                Err(()) => panic!("root future spawned into ThreadPool resolved with error"),
            }
        }
    }
}

impl Notify for _GlobalTask {
    fn notify(&self, _id: usize) {
        if self.signal_level.fetch_add(1, Ordering::SeqCst) == 0 { // TODO: weaken?
            // If the notification if from a worker thread, put the task on its queue
            // TODO: this will work across different ThreadPools. Consider if that is what we want.
            if let Some(current_worker) = CURRENT_WORKER.with(|worker| worker.get()) {
                let current_worker = unsafe { &*current_worker };
                // _GlobalTask is always an Arc. Reconstitute from the pointer
                let global_task = unsafe {
                    let ghost_arc = mem::ManuallyDrop::new(Arc::from_raw(self as *const _GlobalTask));
                    GlobalTask(Arc::clone(&*ghost_arc))
                };
                current_worker.work_queue().push(global_task);
            } else {
                unimplemented!()
            }
        }
    }
}

impl LocalTask {
    fn do_poll(self) {
        let mut init_signal_level = self.0.signal_level.load(Ordering::SeqCst); // TODO: weaken?
        loop {
            match unsafe { (*self.0.spawn.get()).poll_future_notify(&self.0, 0) } {
                Ok(Async::NotReady) => {
                    match self.0.signal_level.compare_exchange(init_signal_level, 0, Ordering::SeqCst, Ordering::SeqCst) { // TODO: weaken?
                        Ok(_) => break,
                        Err(level) => {
                            init_signal_level = level;
                            continue;
                        },
                    }
                },
                // TODO: drop future inside _GlobalTask here, since notify handles may persist
                Ok(Async::Ready(())) => break,
                Err(()) => panic!("root future spawned into ThreadPool resolved with error"),
            }
        }
    }
}

impl Notify for _LocalTask {
    fn notify(&self, _id: usize) {
        if self.signal_level.fetch_add(1, Ordering::SeqCst) == 0 { // TODO: weaken?
            unimplemented!()
        }
    }
}

fn run_worker<Q: EventQueue>(pool: Arc<_ThreadPool<Q>>, event_queue: Arc<Q>) -> Arc<_Worker<Q>> {
    let (queue, stealer) = coco::deque::new();

    let worker = Arc::new(_Worker {
        pool,
        work_queue: stealer,
        join_handle: AtomicOption::new(),
    });

    let join_handle = thread::Builder::new().name("complete_io::ThreadPool::worker".to_string()).spawn({
        let worker = worker.clone();
        move || {
            match panic::catch_unwind(panic::AssertUnwindSafe(|| {
                let worker_self = WorkerSelf {
                    work_queue: queue,
                    worker,
                    local_work_queue: Default::default(),
                };

                CURRENT_WORKER.with(|worker| worker.set(Some(&worker_self)));
                while worker_self.worker.pool.active.load(Ordering::Relaxed) {
                    // We do local work first because other threads cannot steal it
                    while let Some(task) = worker_self.local_work_queue.borrow_mut().pop_front() {
                        task.do_poll();
                    }
                    
                    let should_wait0 = loop {
                        match worker_self.work_queue.steal_weak() {
                            Steal::Data(task) => task.do_poll(),
                            Steal::Empty => break false,
                            Steal::Inconsistent => break true,
                        }
                    };

                    let timeout = if should_wait0 {
                        Some(Duration::from_millis(0))
                    } else {
                        None
                    };

                    event_queue.turn(timeout).expect("failed to turn EventQueue");
                }
                CURRENT_WORKER.with(|worker| worker.set(None));

                let mut workers = worker_self.worker.pool.workers.write().unwrap();
                let index = workers.iter().position(|x| &**x as *const _Worker<Q> == &*worker_self.worker as *const _Worker<Q>).expect("worker reference removed prematurely");
                workers.swap_remove(index);
            })) {
                Ok(()) => (),
                Err(err) => {
                    let message: &str = if let Some(message) = err.downcast_ref::<String>() {
                        message
                    } else if let Some(message) = err.downcast_ref::<&'static str>() {
                        message
                    } else {
                        "Any"
                    };
                    if log_enabled!(::log::LogLevel::Error) {
                        error!("uncaught panic in worker thread: {}", message);
                    } else {
                        eprintln!("uncaught panic in worker thread: {}", message);
                    }
                    ::std::process::exit(101);
                }
            }
        }
    }).expect("failed to create worker thread");

    worker.join_handle.swap(join_handle, Ordering::SeqCst);

    worker
}

fn handle_remote_spawn<Q: EventQueue>(pool: &Arc<_ThreadPool<Q>>, data: usize) {
    unsafe {
        let worker_self = current_worker_handle(pool).expect("custom EventQueue event called on invalid thread");
        let global_task = GlobalTask(Arc::from_raw(data as *const _GlobalTask));

        (*worker_self).work_queue.push(global_task);
    }
}

impl<Q: EventQueue> AsRegistrar<Q> for Handle<Q> {
    fn as_registrar(&self) -> &Q::Handle {
        &self.event_queue_handle
    }
}

impl<Q: EventQueue> AsRegistrar<Q> for LocalHandle<Q> {
    fn as_registrar(&self) -> &Q::Handle {
        &self.global.event_queue_handle
    }
}

impl<Q: EventQueue> Clone for Handle<Q> {
    fn clone(&self) -> Self {
        Handle {
            pool: self.pool.clone(),
            event_queue_handle: self.event_queue_handle.clone(),
        }
    }
}

impl<Q: EventQueue> Clone for LocalHandle<Q> {
    fn clone(&self) -> Self {
        LocalHandle {
            global: self.global.clone(),
            worker_self: self.worker_self,
        }
    }
}

impl<F: Future<Item = (), Error = ()> + Send + 'static, Q: EventQueue> future::Executor<F> for Handle<Q> {
    fn execute(&self, future: F) -> Result<(), future::ExecuteError<F>> {
        self.spawn_future(future);
        Ok(())
    }
}

impl<Q: NetEventQueue> NetEventLoop for ThreadPool<Q> {
    type TcpListener = Q::TcpListener;
    type TcpStream = Q::TcpStream;
    type UdpSocket = Q::UdpSocket;
}

cfg_if! { if #[cfg(all(target_os = "linux", feature = "epoll"))] {
    use ::epoll::Epoll;

    pub type PlatformDefault = ThreadPool<Epoll>;

    impl PlatformDefault {
        pub fn platform_default() -> io::Result<Self> {
            Self::platform_default_with_concurrency(num_cpus::get())
        }

        pub fn platform_default_with_concurrency(concurrency: usize) -> io::Result<Self> {
            let epoll = Epoll::new()?;
            Self::with_concurrency(epoll, concurrency)
        }
    }
} }

#[cfg(test)]
mod tests;