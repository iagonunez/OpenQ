//! Background workers for jobs that read files (probing, stat, waveforms, posters).
//!
//! A read can hang for a long time (a network share that stopped answering, a drive spinning
//! up). With one worker thread that would hold up every other file. A [`Pool`] starts another
//! worker when a job arrives and none is idle (up to its maximum), so jobs for healthy files
//! still go through while a stuck one keeps its thread. Idle workers end after a while.

use parking_lot::{Condvar, Mutex};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

/// How long an idle worker waits for a job before ending.
const IDLE_EXIT: Duration = Duration::from_secs(20);

struct State<T> {
    jobs: VecDeque<T>,
    idle: usize,
    workers: usize,
    closed: bool,
}

struct Inner<T> {
    state: Mutex<State<T>>,
    wake: Condvar,
    name: &'static str,
    max: usize,
    job: Box<dyn Fn(T) + Send + Sync>,
}

/// A queue of jobs run by up to `max` worker threads, started on demand.
pub struct Pool<T: Send + 'static> {
    inner: Arc<Inner<T>>,
}

impl<T: Send + 'static> Pool<T> {
    /// A pool running `job` on threads named `name`; no thread starts before the first job.
    pub fn new(name: &'static str, max: usize, job: impl Fn(T) + Send + Sync + 'static) -> Self {
        let state = State { jobs: VecDeque::new(), idle: 0, workers: 0, closed: false };
        Self { inner: Arc::new(Inner { state: Mutex::new(state), wake: Condvar::new(), name, max: max.max(1), job: Box::new(job) }) }
    }

    /// Queue a job.
    pub fn push(&self, job: T) {
        let mut st = self.inner.state.lock();
        st.jobs.push_back(job);
        if st.idle > 0 {
            self.inner.wake.notify_one();
        } else if st.workers < self.inner.max {
            let inner = self.inner.clone();
            if std::thread::Builder::new().name(self.inner.name.into()).spawn(move || work(inner)).is_ok() {
                st.workers += 1;
            }
        }
    }

    /// Worker threads running now (tests).
    pub fn workers(&self) -> usize {
        self.inner.state.lock().workers
    }
}

impl<T: Send + 'static> Drop for Pool<T> {
    fn drop(&mut self) {
        self.inner.state.lock().closed = true;
        self.inner.wake.notify_all();
    }
}

fn work<T>(inner: Arc<Inner<T>>) {
    let mut st = inner.state.lock();
    loop {
        if st.closed {
            break;
        }
        if let Some(job) = st.jobs.pop_front() {
            drop(st);
            (inner.job)(job);
            st = inner.state.lock();
            continue;
        }
        st.idle += 1;
        let timed_out = inner.wake.wait_for(&mut st, IDLE_EXIT).timed_out();
        st.idle -= 1;
        if timed_out && st.jobs.is_empty() {
            break;
        }
    }
    st.workers -= 1;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A job stuck forever doesn't hold up the others: another worker takes them.
    #[test]
    fn a_stuck_job_does_not_block_the_rest() {
        let release = Arc::new(Mutex::new(()));
        let held = release.lock();
        let done = Arc::new(AtomicUsize::new(0));
        let (r, d) = (release.clone(), done.clone());
        let pool = Pool::new("test-pool", 4, move |stuck: bool| {
            if stuck {
                drop(r.lock());
            }
            d.fetch_add(1, Ordering::SeqCst);
        });
        pool.push(true);
        std::thread::sleep(Duration::from_millis(20));
        for _ in 0..10 {
            pool.push(false);
        }
        let t0 = std::time::Instant::now();
        while done.load(Ordering::SeqCst) < 10 {
            assert!(t0.elapsed() < Duration::from_secs(5), "jobs held up by the stuck one");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(pool.workers() <= 4);
        drop(held);
        while done.load(Ordering::SeqCst) < 11 {
            assert!(t0.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
