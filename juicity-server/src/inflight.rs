use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use juicity_common::protocol::UnderlayAuth;
use tokio::sync::Notify;

/// In-flight key type (32 bytes salt)
pub type InFlightKey = [u8; 32];

/// Manages underlay authentication keys that are in-flight (waiting for their corresponding UDP packets)
///
/// # Sync blocking safety
///
/// This struct uses `std::sync::Mutex` (not `tokio::sync::Mutex`) intentionally:
/// - All critical sections are extremely short (HashMap insert/remove, ~ns level)
/// - No `.await` points are held while the lock is acquired
/// - Using `tokio::sync::Mutex` would add unnecessary overhead for these micro-operations
/// - The lock is never held across an await point, so it cannot cause deadlock
///
/// The `evict()` method does hold the lock across `.await` boundaries in the `tokio::select!`
/// loop, but each individual lock acquisition is released before the `.await` (the lock guard
/// is dropped at the end of each scope), so this is safe.
pub struct InFlightUnderlayKey {
    ttl: Duration,
    inner: Mutex<InFlightInner>,
    notify: Notify,
}

struct InFlightInner {
    keys: HashMap<InFlightKey, UnderlayAuth>,
    timestamps: HashMap<InFlightKey, Instant>,
}

impl InFlightUnderlayKey {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            inner: Mutex::new(InFlightInner {
                keys: HashMap::new(),
                timestamps: HashMap::new(),
            }),
            notify: Notify::new(),
        }
    }

    /// Store an authentication for later retrieval
    pub fn store(&self, key: InFlightKey, auth: UnderlayAuth) {
        let mut inner = self.inner.lock().unwrap();
        inner.keys.insert(key, auth);
        inner.timestamps.insert(key, Instant::now());
        // Notify any waiting evict() call that a new key is available
        self.notify.notify_waiters();
    }

    /// Evict and retrieve an authentication using Notify for zero-latency wakeup.
    /// Uses a loop with notified() to avoid the 100ms sleep penalty.
    pub async fn evict(&self, key: &InFlightKey) -> Option<UnderlayAuth> {
        // First attempt without waiting
        {
            let mut inner = self.inner.lock().unwrap();
            if let Some(auth) = inner.keys.remove(key) {
                inner.timestamps.remove(key);
                return Some(auth);
            }
        }

        // If not found yet, wait for notification with a short timeout
        // to handle the case where the key never arrives.
        // We use a loop to re-check after notification, since notify_waiters()
        // wakes ALL waiters and our key might not be the one that arrived.
        let deadline = Instant::now() + Duration::from_millis(100);
        loop {
            let wait = self.notify.notified();
            tokio::select! {
                _ = wait => {
                    // Woken up - check if our key arrived
                    let mut guard = self.inner.lock().unwrap();
                    if let Some(auth) = guard.keys.remove(key) {
                        guard.timestamps.remove(key);
                        return Some(auth);
                    }
                    // Not our key, loop back to wait again (if within deadline)
                    if Instant::now() >= deadline {
                        return None;
                    }
                }
                _ = tokio::time::sleep_until(deadline.into()) => {
                    // Timeout - try one last time
                    let mut guard = self.inner.lock().unwrap();
                    let auth = guard.keys.remove(key);
                    guard.timestamps.remove(key);
                    return auth;
                }
            }
        }
    }

    /// Clean up expired keys. Uses split-borrow retain to avoid intermediate Vec allocation.
    pub fn cleanup(&self) {
        let mut inner = self.inner.lock().unwrap();
        let now = Instant::now();
        let ttl = self.ttl;
        let InFlightInner { keys, timestamps } = &mut *inner;
        keys.retain(|k, _| timestamps.get(k).map_or(false, |ts| now.duration_since(*ts) <= ttl));
        timestamps.retain(|_, ts| now.duration_since(*ts) <= ttl);
    }
}
