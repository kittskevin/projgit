//! Single-flight coalescing.
//!
//! Concurrent calls to [`Coalescer::do_or_join`] for the same key
//! result in exactly one execution of `f`; all other callers block
//! until the in-flight call completes and then receive a clone of its
//! result.
//!
//! Implementation is built on stdlib `Mutex` + `Condvar` to avoid
//! pulling in tokio or any async runtime.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Condvar, Mutex};

/// A `(K, V)` single-flight cache that **does not** memoize results.
///
/// Once a fetch completes (success or failure), the entry is removed
/// so the *next* call for the same key starts fresh. This matches the
/// Fetcher semantics: subsequent reads should re-attempt rather than
/// remember a stale failure.
pub struct Coalescer<K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    inflight: Mutex<HashMap<K, Arc<Slot<V>>>>,
}

struct Slot<V: Clone> {
    state: Mutex<SlotState<V>>,
    cv: Condvar,
}

enum SlotState<V: Clone> {
    Pending,
    Done(Result<V, String>),
}

impl<K, V> Coalescer<K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    /// Create an empty coalescer.
    pub fn new() -> Self {
        Self {
            inflight: Mutex::new(HashMap::new()),
        }
    }

    /// Run `f()` if no other thread is already running it for `key`;
    /// otherwise block on the in-flight call and return a clone of its
    /// result.
    ///
    /// `f`'s error type is collapsed to `String` for transport across
    /// thread boundaries; the caller must re-wrap it into a domain
    /// error if needed.
    pub fn do_or_join<F, E>(&self, key: K, f: F) -> Result<V, String>
    where
        F: FnOnce() -> Result<V, E>,
        E: std::fmt::Display,
    {
        // 1. Try to install a new pending slot, or get a reference to
        //    an existing one.
        let (slot, leader) = {
            let mut map = self.inflight.lock().unwrap();
            if let Some(existing) = map.get(&key) {
                (existing.clone(), false)
            } else {
                let slot = Arc::new(Slot {
                    state: Mutex::new(SlotState::Pending),
                    cv: Condvar::new(),
                });
                map.insert(key.clone(), slot.clone());
                (slot, true)
            }
        };

        if leader {
            // 2a. We are the leader: do the work, store the result,
            //     wake everyone, then evict the slot from the map.
            let result = f().map_err(|e| e.to_string());
            {
                let mut state = slot.state.lock().unwrap();
                *state = SlotState::Done(result.clone());
                slot.cv.notify_all();
            }
            self.inflight.lock().unwrap().remove(&key);
            result
        } else {
            // 2b. We are a follower: wait for the leader to finish.
            let mut state = slot.state.lock().unwrap();
            while matches!(*state, SlotState::Pending) {
                state = slot.cv.wait(state).unwrap();
            }
            match &*state {
                SlotState::Done(r) => r.clone(),
                SlotState::Pending => unreachable!("loop above breaks on Done"),
            }
        }
    }

    /// How many keys currently have an in-flight call. Test-facing.
    #[allow(dead_code)]
    pub fn inflight_count(&self) -> usize {
        self.inflight.lock().unwrap().len()
    }
}

impl<K, V> Default for Coalescer<K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn distinct_keys_run_independently() {
        let c: Coalescer<i32, i32> = Coalescer::new();
        let r1: Result<i32, String> = c.do_or_join(1, || Ok::<_, String>(11));
        let r2: Result<i32, String> = c.do_or_join(2, || Ok::<_, String>(22));
        assert_eq!(r1.unwrap(), 11);
        assert_eq!(r2.unwrap(), 22);
        // Slot is removed after each call.
        assert_eq!(c.inflight_count(), 0);
    }

    #[test]
    fn concurrent_same_key_runs_once() {
        let c: Arc<Coalescer<i32, i32>> = Arc::new(Coalescer::new());
        let calls = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..16 {
            let c = c.clone();
            let calls = calls.clone();
            handles.push(thread::spawn(move || {
                c.do_or_join(42, || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(50));
                    Ok::<i32, String>(7)
                })
            }));
        }
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        assert_eq!(calls.load(Ordering::SeqCst), 1, "f should run exactly once");
        assert!(results.iter().all(|r| r.as_ref().ok() == Some(&7)));
        assert_eq!(c.inflight_count(), 0);
    }

    #[test]
    fn errors_are_propagated_to_followers() {
        let c: Arc<Coalescer<i32, i32>> = Arc::new(Coalescer::new());
        let mut handles = Vec::new();
        for _ in 0..4 {
            let c = c.clone();
            handles.push(thread::spawn(move || {
                c.do_or_join(99, || {
                    thread::sleep(Duration::from_millis(20));
                    Err::<i32, String>("boom".to_owned())
                })
            }));
        }
        for h in handles {
            let r = h.join().unwrap();
            assert_eq!(r, Err("boom".to_owned()));
        }
    }

    #[test]
    fn failure_does_not_memoize() {
        let c: Coalescer<i32, i32> = Coalescer::new();
        let calls = AtomicUsize::new(0);

        let _r1: Result<i32, String> = c.do_or_join(5, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Err::<i32, String>("first".to_owned())
        });
        let _r2: Result<i32, String> = c.do_or_join(5, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok::<i32, String>(123)
        });
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
