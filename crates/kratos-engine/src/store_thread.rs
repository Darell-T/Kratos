//! The thread that owns the store.
//!
//! One thread, the engine's main thread, holds the [`Store`] and runs jobs against it one at a
//! time, in the order they arrive. Async code reaches it through a [`StoreThread`] handle and
//! awaits each job's result, so no tokio worker ever blocks on SQLite, whose commits wait for the
//! disk. Nothing else touches the connection, so the store needs no lock.

use kratos_store::Store;
use tokio::sync::{mpsc, oneshot};

/// Jobs that may wait for the store. When the queue is full, callers wait asynchronously for room:
/// the requests that caused the work slow down, and no work is dropped.
const QUEUED_JOBS: usize = 64;

type Job = Box<dyn FnOnce(&mut Store) + Send>;

pub fn channel() -> (StoreThread, StoreJobs) {
    let (jobs, queue) = mpsc::channel(QUEUED_JOBS);
    (StoreThread { jobs }, StoreJobs { queue })
}

#[derive(Clone)]
pub struct StoreThread {
    jobs: mpsc::Sender<Job>,
}

impl StoreThread {
    /// A job that has been queued runs to its end even when the caller stops waiting, because the
    /// caller's task was cancelled. A job that commits and then notifies therefore never does the
    /// first without the second.
    pub async fn call<T, F>(&self, job: F) -> T
    where
        F: FnOnce(&mut Store) -> T + Send + 'static,
        T: Send + 'static,
    {
        let (reply, result) = oneshot::channel();
        let job: Job = Box::new(move |store| {
            let _ = reply.send(job(store));
        });
        self.jobs
            .send(job)
            .await
            .expect("the store thread outlives its handles");
        result
            .await
            .expect("the store thread finishes every job it takes")
    }
}

pub struct StoreJobs {
    queue: mpsc::Receiver<Job>,
}

impl StoreJobs {
    /// Runs jobs on the calling thread until every [`StoreThread`] handle is dropped, then drops
    /// the store, which closes the database and releases the data directory.
    pub fn run(mut self, mut store: Store) {
        while let Some(job) = self.queue.blocking_recv() {
            job(&mut store);
        }
    }
}
