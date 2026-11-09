//! A clock the tests move by hand, so a limit that resets in hours is reached without waiting. It
//! is the clock of `tests/codex/usage/scheduler.rs`, copied because each test binary has its own
//! harness.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use kratos_engine::scheduler::Clock;
use tokio::sync::watch;

use crate::harness::WAIT;

/// When the spent window resets, in seconds since the epoch: 2100-01-01.
pub const RESET: u64 = 4_102_444_800;
pub const MINUTE: u64 = 60;
pub const HOUR: u64 = 3_600;

pub fn at(seconds: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
}

/// A clock that moves only when the test moves it, and remembers every wake-up asked of it.
pub struct ManualClock {
    now: watch::Sender<SystemTime>,
    asked: Mutex<Vec<SystemTime>>,
}

impl ManualClock {
    pub fn starting_at(now: SystemTime) -> Arc<ManualClock> {
        Arc::new(ManualClock {
            now: watch::channel(now).0,
            asked: Mutex::default(),
        })
    }

    pub fn advance_to(&self, now: SystemTime) {
        self.now.send_replace(now);
    }

    pub async fn wait_for_wake_up_at(&self, wake: SystemTime) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while !self
            .asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&wake)
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the scheduler never asked to wake at {wake:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

impl Clock for ManualClock {
    fn now(&self) -> SystemTime {
        *self.now.borrow()
    }

    fn sleep_until(&self, wake: SystemTime) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(wake);
        let mut now = self.now.subscribe();
        Box::pin(async move {
            while *now.borrow_and_update() < wake {
                if now.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
        })
    }
}
