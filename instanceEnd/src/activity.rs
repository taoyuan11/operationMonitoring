use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use tokio::sync::{Notify, watch};

#[derive(Clone, Default)]
pub struct ActivityTracker {
    state: Arc<ActivityState>,
}

struct ActivityState {
    active: AtomicUsize,
    draining: AtomicBool,
    draining_updates: watch::Sender<bool>,
    idle: Notify,
}

impl Default for ActivityState {
    fn default() -> Self {
        let (draining_updates, _) = watch::channel(false);
        Self {
            active: AtomicUsize::default(),
            draining: AtomicBool::default(),
            draining_updates,
            idle: Notify::default(),
        }
    }
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
        self.set_draining(true);
    }

    pub fn stop_draining(&self) {
        self.set_draining(false);
    }

    pub fn subscribe_draining(&self) -> watch::Receiver<bool> {
        self.state.draining_updates.subscribe()
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

    fn set_draining(&self, draining: bool) {
        self.state.draining_updates.send_if_modified(|current| {
            self.state.draining.store(draining, Ordering::SeqCst);
            if *current == draining {
                false
            } else {
                *current = draining;
                true
            }
        });
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

    #[tokio::test]
    async fn subscribers_observe_draining_state_changes() {
        let tracker = ActivityTracker::default();
        let mut draining = tracker.subscribe_draining();

        assert!(!*draining.borrow());

        tracker.start_draining();
        draining.changed().await.unwrap();
        assert!(*draining.borrow_and_update());

        tracker.stop_draining();
        draining.changed().await.unwrap();
        assert!(!*draining.borrow_and_update());
    }
}
