use ::evloop::{EventLoop, Handle};

use futures;
use futures::prelude::*;

#[test]
fn spawn_remote() {
    let thread_pool = super::ThreadPool::platform_default().unwrap();
    let (tx, rx) = futures::sync::oneshot::channel();
    thread_pool.handle().spawn(move |_| {
        tx.send(1).unwrap();
        Ok(())
    });

    let value = rx.wait().unwrap();
    assert_eq!(1, value);

}

make_net_loop_tests!(::evloop::threadpool::ThreadPool::platform_default().unwrap());