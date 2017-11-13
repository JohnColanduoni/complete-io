pub mod net;

use ::queue::{self, EventQueue, CustomEventHandler};

use std::{io, mem};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::os::windows::prelude::*;

use futures::prelude::*;
use futures::task::AtomicTask;
use miow::Overlapped;
use miow::iocp::{CompletionPort as RawCompletionPort, CompletionStatus};
use winapi::*;
use kernel32::*;
use ws2_32::*;

#[derive(Clone)]
pub struct CompletionPort {
    inner: Arc<Inner>,
}

struct Inner {
    iocp: RawCompletionPort,
    event_table: Mutex<Vec<Box<CustomEventHandler<CompletionPort>>>>,
}

#[derive(Clone)]
pub struct Handle {
    inner: Arc<Inner>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct EventTag {
    callback_ptr: usize,
    iocp_id: usize, // The ID is used to make sure we only queue custom events on the right CompletionPort
}

impl CompletionPort {
    pub fn new(max_concurrency: u32) -> io::Result<Self> {
        let inner = Arc::new(Inner {
            iocp: RawCompletionPort::new(max_concurrency)?,
            event_table: Default::default(),
        });

        Ok(CompletionPort { inner })
    }
}

impl EventQueue for CompletionPort {
    type Handle = Handle;
    type EventTag = EventTag;

    fn handle(&self) -> Handle { Handle { inner: self.inner.clone() } }

    fn turn(&mut self, max_wait: Option<Duration>) -> io::Result<usize> {
        self.inner.poll_iocp(max_wait)
    }

    fn new_custom_event(&self, handler: CustomEventHandler<Self>) -> io::Result<Self::EventTag> {
        let mut event_table = self.inner.event_table.lock().unwrap();
        // We double box the handler so it can be posted as a single pointer
        let handler_double_boxed = Box::new(handler);
        let callback_ptr = &*handler_double_boxed as *const _ as usize;
        event_table.push(handler_double_boxed);
        Ok(EventTag {
            callback_ptr,
            iocp_id: self.inner.iocp.as_raw_handle() as usize,
        })
    }
}

impl Handle {
    pub fn add_socket<S: AsRawSocket + ?Sized>(&self, socket: &S) -> io::Result<()> {
        self.inner.iocp.add_socket(FUTURE_NOTIFY_IOCP_TOKEN, socket)
    }
}

impl queue::Handle for Handle {
    type EventQueue = CompletionPort;

    fn post_custom_event(&self, event: EventTag, data: usize) -> io::Result<()> {
        if event.iocp_id != self.inner.iocp.as_raw_handle() as usize {
            panic!("the given EventTag is not for this IOCP");
        }
        self.inner.iocp.post(CompletionStatus::new(0, event.callback_ptr, data as _))
    }
}

impl queue::EventTag for EventTag {
    type EventQueue = CompletionPort;
}

/// Allows overlapped IO operations to `notify` tasks.
/// 
/// This structure need only be used when adding bindings to additional objects capable
/// of overlapped IO. When passed to Windows APIs like `WSASend` or `ReadFile`, this
/// overlapped will ensure the given task is notified when the operation completes. Note
/// that you must ensure the IO object is added to the IOCP prior, or no notifications will
/// be received.
#[derive(Clone)]
pub struct OverlappedTask(Arc<_OverlappedTask>);

#[repr(C)] // The pointer for Overlapped and _OverlappedTask must be the same
struct _OverlappedTask {
    overlapped: Overlapped,
    task: AtomicTask,
}

impl OverlappedTask {
    /// Creates an `OverlappedTask` that will notify the current task.
    pub fn new(_evloop: &Handle) -> OverlappedTask {
        let overlapped = Arc::new(_OverlappedTask {
            overlapped: Overlapped::zero(),
            task: AtomicTask::new(),
        });
        overlapped.task.register();
        OverlappedTask(overlapped)
    }

    /// Alters the registered `Task` associated with this instance. This behaves like `AtomicTask::register`.
    pub fn register(&self) {
        self.0.task.register();
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
    pub unsafe fn for_operation<F, R>(self, f: F) -> io::Result<R>
        where F: FnOnce(*mut OVERLAPPED) -> io::Result<R>
    {
        let overlapped = self.0.overlapped.raw();
        match f(overlapped) {
            Ok(x) => {
                mem::forget(self); // Hand our +1 refcount to the IOCP
                Ok(x)
            },
            Err(err) => Err(err),
        }
    }

    pub fn poll_socket<S: AsRawSocket>(&self, socket: &S) -> Poll<(usize, DWORD), io::Error> {
        match unsafe { (*self.0.overlapped.raw()).Internal as i32 } {
            STATUS_PENDING => Ok(Async::NotReady),
            _ => {
                let mut transferred: DWORD = 0;
                let mut flags: DWORD = 0;
                if unsafe { WSAGetOverlappedResult(
                    socket.as_raw_socket(),
                    self.0.overlapped.raw(),
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

    pub fn cancel_socket<S: AsRawSocket>(self, socket: &S) -> Result<(), (Self, io::Error)> {
        unsafe { winapi_bool_call!(log: CancelIoEx(
            socket.as_raw_socket() as _,
            self.0.overlapped.raw(),
        )).map_err(|err| (self, err)) }
    }
}

const FUTURE_NOTIFY_IOCP_TOKEN: usize = 1;

impl Inner {
    fn poll_iocp(&self, timeout: Option<Duration>) -> io::Result<usize> {
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
                        return Ok(0);
                    },
                    // Local tasks are queued as APCs, which will cause the call to be interrupted with WAIT_IO_COMPLETION
                    WAIT_IO_COMPLETION => {
                        unimplemented!()
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
                FUTURE_NOTIFY_IOCP_TOKEN => {
                    let overlapped_task = unsafe { Arc::from_raw(completion.overlapped() as *const _OverlappedTask) }; // Submission to IO operation added a +1 to retain count
                    overlapped_task.task.notify();
                },
                custom => {
                    let callback = unsafe { &*(custom as *const CustomEventHandler<CompletionPort>) };
                    let event_tag = EventTag {
                        callback_ptr: custom,
                        iocp_id: self.iocp.as_raw_handle() as usize,
                    };
                    callback(event_tag, completion.overlapped() as usize);
                },
            }
        }

        Ok(completions.len())
    }
}
