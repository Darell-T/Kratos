//! Wall-clock time, and waiting for it. Tests move a clock by hand instead of waiting hours.

use std::future::Future;
use std::pin::Pin;
use std::time::SystemTime;

pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> SystemTime;
    /// Resolves at `at`, or at once when `at` has passed.
    fn sleep_until(&self, at: SystemTime) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }

    fn sleep_until(&self, at: SystemTime) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let wait = at.duration_since(SystemTime::now()).unwrap_or_default();
        Box::pin(tokio::time::sleep(wait))
    }
}
