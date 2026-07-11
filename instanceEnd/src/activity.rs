use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use tokio::sync::Notify;

#[derive(Clone, Default)]
pub struct ActivityTracker {
    state: Arc<ActivityState>,
}

#[derive(Default)]
struct ActivityState {
    active: AtomicUsize,
    draining: AtomicBool,
    idle: Notify,
}

pub struct ActivityGuard {
    tracker: ActivityTracker,
}

impl ActivityTracker {
    pub fn try_enter(&self) -> Option<ActivityGuard> {
        if self.state.draining.load(Ordering::SeqCst) {
            return None;
        }

        self.state.active.fetch_add(1, Ordering::SeqCst);
        if self.state.draining.load(Ordering::SeqCst) {
            self.leave();
            return None;
        }

        Some(ActivityGuard {
            tracker: self.clone(),
        })
    }

    pub fn start_draining(&self) {
        self.state.draining.store(true, Ordering::SeqCst);
    }

    pub fn stop_draining(&self) {
        self.state.draining.store(false, Ordering::SeqCst);
    }

    pub fn active_count(&self) -> usize {
        self.state.active.load(Ordering::SeqCst)
    }

    pub async fn wait_until_idle(&self) {
        loop {
            let notified = self.state.idle.notified();
            if self.active_count() == 0 {
                return;
            }
            notified.await;
        }
    }

    fn leave(&self) {
        if self.state.active.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.state.idle.notify_waiters();
        }
    }
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        self.tracker.leave();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn draining_rejects_new_work_and_waits_for_existing_work() {
        let tracker = ActivityTracker::default();
        let guard = tracker.try_enter().unwrap();
        tracker.start_draining();

        assert!(tracker.try_enter().is_none());
        assert_eq!(tracker.active_count(), 1);

        drop(guard);
        tracker.wait_until_idle().await;
        assert_eq!(tracker.active_count(), 0);

        tracker.stop_draining();
        assert!(tracker.try_enter().is_some());
    }
}
