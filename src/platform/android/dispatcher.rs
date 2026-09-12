//! The executor: a pool of background threads, a timer thread, and a queue
//! of main-thread runnables drained by the event loop in `platform.rs`,
//! which the activity's looper is woken for.

use crate::{PlatformDispatcher, TaskLabel};
use android_activity::AndroidAppWaker;
use async_task::Runnable;
use std::{
    cmp::Reverse,
    collections::BinaryHeap,
    thread,
    time::{Duration, Instant},
};

struct TimerAfter {
    deadline: Instant,
    runnable: Runnable,
}

impl PartialEq for TimerAfter {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline
    }
}

impl Eq for TimerAfter {}

impl PartialOrd for TimerAfter {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TimerAfter {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.deadline.cmp(&other.deadline)
    }
}

pub(crate) struct AndroidDispatcher {
    main_sender: flume::Sender<Runnable>,
    /// Wakes the activity's looper; a headless process has none.
    waker: Option<AndroidAppWaker>,
    timer_sender: flume::Sender<TimerAfter>,
    background_sender: flume::Sender<Runnable>,
    main_thread_id: thread::ThreadId,
}

impl AndroidDispatcher {
    /// `main_sender` feeds the queue the event loop drains; `waker` gets
    /// the loop out of its poll when something lands there.
    pub(crate) fn new(
        main_sender: flume::Sender<Runnable>,
        waker: Option<AndroidAppWaker>,
    ) -> Self {
        let (background_sender, background_receiver) = flume::unbounded::<Runnable>();
        let thread_count = thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(2)
            .max(2);
        for i in 0..thread_count {
            let receiver = background_receiver.clone();
            thread::Builder::new()
                .name(format!("Worker-{i}"))
                .spawn(move || {
                    for runnable in receiver {
                        runnable.run();
                    }
                })
                .expect("failed to spawn a background thread");
        }

        let (timer_sender, timer_receiver) = flume::unbounded::<TimerAfter>();
        thread::Builder::new()
            .name("Timer".to_owned())
            .spawn(move || {
                let mut timers: BinaryHeap<Reverse<TimerAfter>> = BinaryHeap::new();
                loop {
                    let now = Instant::now();
                    while timers
                        .peek()
                        .is_some_and(|Reverse(timer)| timer.deadline <= now)
                    {
                        if let Some(Reverse(timer)) = timers.pop() {
                            timer.runnable.run();
                        }
                    }
                    let received = match timers.peek() {
                        Some(Reverse(next)) => {
                            match timer_receiver.recv_timeout(next.deadline - Instant::now()) {
                                Ok(timer) => Some(timer),
                                Err(flume::RecvTimeoutError::Timeout) => None,
                                Err(flume::RecvTimeoutError::Disconnected) => return,
                            }
                        }
                        None => match timer_receiver.recv() {
                            Ok(timer) => Some(timer),
                            Err(_) => return,
                        },
                    };
                    if let Some(timer) = received {
                        timers.push(Reverse(timer));
                    }
                }
            })
            .expect("failed to spawn the timer thread");

        Self {
            main_sender,
            waker,
            timer_sender,
            background_sender,
            main_thread_id: thread::current().id(),
        }
    }
}

impl PlatformDispatcher for AndroidDispatcher {
    fn is_main_thread(&self) -> bool {
        thread::current().id() == self.main_thread_id
    }

    fn dispatch(&self, runnable: Runnable, _: Option<TaskLabel>) {
        self.background_sender.send(runnable).ok();
    }

    fn dispatch_on_main_thread(&self, runnable: Runnable) {
        match self.main_sender.send(runnable) {
            Ok(()) => {
                if let Some(waker) = &self.waker {
                    waker.wake();
                }
            }
            Err(flume::SendError(runnable)) => {
                // The loop is gone, so the app is shutting down. The
                // runnable may wrap a future that is not `Send`, which must
                // not be dropped on this thread.
                std::mem::forget(runnable);
            }
        }
    }

    fn dispatch_after(&self, duration: Duration, runnable: Runnable) {
        self.timer_sender
            .send(TimerAfter {
                deadline: Instant::now() + duration,
                runnable,
            })
            .ok();
    }
}
