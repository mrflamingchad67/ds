//! Bounded work queue for directory traversal.
//!
//! A drive can hold millions of directories, so the scanner must never spawn a
//! thread per directory. Workers instead pull from this queue, which caps how
//! many directories can be in flight and refuses growth once the cap is hit.
//!
//! # Lifetime
//!
//! The queue closes itself once the last outstanding directory has been
//! *finished*, not merely taken: a worker processing a directory still counts as
//! outstanding, because it may yet push children. Without that distinction a
//! worker could push into an already-closed queue and silently lose a subtree.
//!
//! [`WorkQueue::abort`] is the escape hatch for cancellation, waking every
//! blocked worker.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Condvar, Mutex, MutexGuard};

/// Default cap on queued-but-unclaimed directories.
///
/// Sized well above the worker count so workers rarely stall, but small enough
/// that a pathological tree cannot exhaust memory.
pub const DEFAULT_CAPACITY: usize = 16_384;

/// A multi-producer, multi-consumer queue of directories to visit.
#[derive(Debug)]
pub struct WorkQueue {
    state: Mutex<State>,
    not_empty: Condvar,
    not_full: Condvar,
    capacity: usize,
}

#[derive(Debug, Default)]
struct State {
    items: VecDeque<PathBuf>,
    /// Directories queued but not yet reported complete, including those a
    /// worker is currently processing.
    outstanding: usize,
    /// Set once no more work will arrive; workers drain and then exit.
    closed: bool,
}

impl WorkQueue {
    /// Create a queue holding at most `capacity` queued directories.
    ///
    /// A capacity of zero is treated as one, since a queue that can never hold
    /// work would stall every worker.
    pub fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(State::default()),
            not_empty: Condvar::new(),
            not_full: Condvar::new(),
            capacity: capacity.max(1),
        }
    }

    /// Queue a directory, blocking while the queue is full.
    ///
    /// Returns `false` if the queue was closed or aborted while waiting, in
    /// which case the directory was not queued and the caller must handle it.
    pub fn push(&self, path: PathBuf) -> bool {
        let mut state = self.lock();

        while state.items.len() >= self.capacity && !state.closed {
            state = self.wait_not_full(state);
        }

        if state.closed {
            return false;
        }

        state.items.push_back(path);
        state.outstanding += 1;
        drop(state);
        self.not_empty.notify_one();
        true
    }

    /// Queue a directory without blocking.
    ///
    /// Returns `false` when the queue is full or closed. Callers use this to
    /// fall back to handling the directory themselves rather than stalling a
    /// worker that may be the only one able to drain the queue.
    pub fn try_push(&self, path: PathBuf) -> bool {
        self.try_push_or_return(path).is_ok()
    }

    /// Like [`WorkQueue::try_push`], but hands the path back when refused.
    ///
    /// This lets a worker that overflows the queue take ownership of the
    /// directory again without cloning the `PathBuf`.
    pub fn try_push_or_return(&self, path: PathBuf) -> Result<(), PathBuf> {
        let mut state = self.lock();

        if state.closed || state.items.len() >= self.capacity {
            return Err(path);
        }

        state.items.push_back(path);
        state.outstanding += 1;
        drop(state);
        self.not_empty.notify_one();
        Ok(())
    }

    /// Take the next directory, blocking until one arrives or the queue closes.
    ///
    /// Returns `None` once the queue is closed and fully drained. The caller
    /// must call [`WorkQueue::complete`] once the directory is finished.
    pub fn pop(&self) -> Option<PathBuf> {
        let mut state = self.lock();

        loop {
            if let Some(path) = state.items.pop_front() {
                drop(state);
                self.not_full.notify_one();
                return Some(path);
            }

            if state.closed {
                return None;
            }

            state = self.wait_not_empty(state);
        }
    }

    /// Report that a previously popped directory is finished.
    ///
    /// When the last outstanding directory completes, the queue closes itself
    /// and wakes every waiting worker.
    pub fn complete(&self) {
        let mut state = self.lock();
        state.outstanding = state.outstanding.saturating_sub(1);
        let drained = state.outstanding == 0;

        if drained {
            state.closed = true;
        }

        drop(state);

        if drained {
            self.not_empty.notify_all();
            self.not_full.notify_all();
        }
    }

    /// Close the queue, waking every blocked worker.
    ///
    /// Already-queued directories are still drained, so a normal shutdown does
    /// not abandon work.
    pub fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        drop(state);
        self.not_empty.notify_all();
        self.not_full.notify_all();
    }

    /// Discard queued work and close, used when cancelling.
    pub fn abort(&self) {
        let mut state = self.lock();
        state.items.clear();
        state.outstanding = 0;
        state.closed = true;
        drop(state);
        self.not_empty.notify_all();
        self.not_full.notify_all();
    }

    /// Number of directories currently waiting to be popped.
    pub fn len(&self) -> usize {
        self.lock().items.len()
    }

    /// Whether no directories are waiting.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Lock the state, recovering from a poisoned mutex.
    ///
    /// A panic in one worker must not stop the rest of the scan, so poisoning
    /// is deliberately ignored throughout.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn wait_not_empty<'a>(&self, guard: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        self.not_empty
            .wait(guard)
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn wait_not_full<'a>(&self, guard: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        self.not_full
            .wait(guard)
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for WorkQueue {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    fn path(n: u32) -> PathBuf {
        PathBuf::from(format!("dir-{n}"))
    }

    #[test]
    fn push_then_pop_returns_the_same_item() {
        let queue = WorkQueue::new(4);
        assert!(queue.push(path(1)));
        assert_eq!(queue.pop(), Some(path(1)));
        queue.complete();
    }

    #[test]
    fn the_queue_closes_once_all_work_completes() {
        let queue = Arc::new(WorkQueue::new(4));
        assert!(queue.push(path(1)));

        let consumer = {
            let queue = Arc::clone(&queue);
            thread::spawn(move || {
                while let Some(item) = queue.pop() {
                    assert_eq!(item, path(1));
                    queue.complete();
                }
                "drained"
            })
        };

        assert_eq!(
            consumer.join().expect("consumer should not panic"),
            "drained"
        );
    }

    #[test]
    fn close_still_drains_queued_work() {
        let queue = WorkQueue::new(4);
        queue.push(path(1));
        queue.close();
        assert_eq!(queue.pop(), Some(path(1)));
        queue.complete();
        assert_eq!(queue.pop(), None);
    }

    #[test]
    fn push_is_refused_once_closed() {
        let queue = WorkQueue::new(4);
        queue.close();
        assert!(!queue.push(path(1)));
    }

    #[test]
    fn work_in_flight_keeps_the_queue_open() {
        // A worker that has popped a directory but not finished it must be able
        // to push children. Otherwise subtrees are silently lost.
        let queue = WorkQueue::new(8);
        assert!(queue.push(path(1)));
        assert_eq!(queue.pop(), Some(path(1)));

        // Nothing else is queued and nothing is outstanding besides path(1).
        assert!(
            queue.try_push(path(2)),
            "in-flight work must keep queue open"
        );

        queue.complete();
        assert_eq!(queue.pop(), Some(path(2)));
        queue.complete();
        assert_eq!(
            queue.pop(),
            None,
            "queue closes when the last item finishes"
        );
    }

    #[test]
    fn complete_is_idempotent_under_saturation() {
        let queue = WorkQueue::new(4);
        queue.complete();
        assert_eq!(queue.pop(), None);
    }

    #[test]
    fn capacity_never_exceeds_the_limit() {
        let queue = WorkQueue::new(3);
        for n in 0..3 {
            assert!(queue.push(path(n)));
        }
        assert_eq!(queue.len(), 3);
        assert!(!queue.try_push(path(99)), "a full queue must refuse");
        assert_eq!(queue.len(), 3);
    }

    #[test]
    fn a_blocked_push_proceeds_once_a_slot_frees() {
        let queue = Arc::new(WorkQueue::new(1));
        assert!(queue.push(path(1)));

        let producer = {
            let queue = Arc::clone(&queue);
            thread::spawn(move || queue.push(path(2)))
        };

        assert_eq!(queue.pop(), Some(path(1)));
        assert!(producer.join().expect("producer should not panic"));
        assert_eq!(queue.pop(), Some(path(2)));
        queue.complete();
        queue.complete();
    }

    #[test]
    fn a_blocked_push_is_released_by_close() {
        let queue = Arc::new(WorkQueue::new(1));
        assert!(queue.push(path(1)));

        let producer = {
            let queue = Arc::clone(&queue);
            thread::spawn(move || queue.push(path(2)))
        };

        // Let the producer actually block on the full queue.
        thread::sleep(Duration::from_millis(20));
        queue.close();

        assert!(
            !producer.join().expect("producer should not panic"),
            "a closed queue must refuse the blocked push"
        );
    }

    #[test]
    fn a_blocked_pop_is_released_by_close() {
        let queue = Arc::new(WorkQueue::new(4));
        let consumer = {
            let queue = Arc::clone(&queue);
            thread::spawn(move || queue.pop())
        };

        thread::sleep(Duration::from_millis(20));
        queue.close();
        assert_eq!(consumer.join().expect("consumer should not panic"), None);
    }

    #[test]
    fn abort_discards_pending_work() {
        let queue = WorkQueue::new(8);
        queue.push(path(1));
        queue.push(path(2));
        assert_eq!(queue.len(), 2);

        queue.abort();
        assert!(queue.is_empty(), "pending work is dropped");
        assert_eq!(queue.pop(), None, "an aborted queue yields nothing");
    }

    #[test]
    fn abort_wakes_a_waiter_idling_on_an_empty_queue() {
        // A worker blocked with nothing to do must wake on abort, otherwise the
        // pool would never shut down.
        let queue = Arc::new(WorkQueue::new(8));
        queue.push(path(1));

        let drainer = {
            let queue = Arc::clone(&queue);
            thread::spawn(move || {
                // Takes the one outstanding item, then idles.
                let _ = queue.pop();
                queue.complete();
                queue.pop()
            })
        };

        thread::sleep(Duration::from_millis(50));
        queue.abort();

        assert_eq!(
            drainer.join().expect("drainer should not panic"),
            None,
            "the idle pop must return once aborted"
        );
    }

    #[test]
    fn abort_unblocks_a_push_waiter() {
        let queue = Arc::new(WorkQueue::new(1));
        queue.push(path(1));

        let producer = {
            let queue = Arc::clone(&queue);
            thread::spawn(move || queue.push(path(2)))
        };

        thread::sleep(Duration::from_millis(20));
        queue.abort();
        assert!(!producer.join().expect("producer should not panic"));
    }

    #[test]
    fn zero_capacity_is_clamped_to_one() {
        let queue = WorkQueue::new(0);
        assert!(queue.push(path(1)));
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn producers_and_consumers_deliver_every_item_exactly_once() {
        const PRODUCERS: u32 = 4;
        const PER_PRODUCER: u32 = 500;
        const CONSUMERS: u32 = 8;
        const TOTAL: u32 = PRODUCERS * PER_PRODUCER;

        // The queue is large enough to hold everything, so producers never
        // block and cannot be cut off by the auto-close that a drained queue
        // triggers. Blocking producers are covered by their own tests.
        let queue = Arc::new(WorkQueue::new(TOTAL as usize * 2));
        let consumed = Arc::new(Mutex::new(Vec::new()));

        let producers: Vec<_> = (0..PRODUCERS)
            .map(|p| {
                let queue = Arc::clone(&queue);
                thread::spawn(move || {
                    for n in 0..PER_PRODUCER {
                        assert!(queue.push(path(p * PER_PRODUCER + n)));
                    }
                })
            })
            .collect();

        for producer in producers {
            producer.join().expect("producer should not panic");
        }

        let consumers: Vec<_> = (0..CONSUMERS)
            .map(|_| {
                let queue = Arc::clone(&queue);
                let consumed = Arc::clone(&consumed);
                thread::spawn(move || {
                    while let Some(item) = queue.pop() {
                        consumed
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .push(item);
                        queue.complete();
                    }
                })
            })
            .collect();

        for consumer in consumers {
            consumer.join().expect("consumer should not panic");
        }

        let mut seen = consumed.lock().unwrap_or_else(|p| p.into_inner()).clone();
        seen.sort();

        // Built in numeric order, so it must be sorted the same way before
        // comparing: "dir-10" precedes "dir-2" lexicographically.
        let mut expected: Vec<PathBuf> = (0..TOTAL).map(path).collect();
        expected.sort();

        assert_eq!(
            seen.len(),
            expected.len(),
            "every item must be delivered exactly once"
        );
        assert_eq!(seen, expected, "delivered items must match what was pushed");
    }

    #[test]
    fn default_queue_has_the_documented_capacity() {
        assert_eq!(WorkQueue::default().capacity, DEFAULT_CAPACITY);
    }
}
