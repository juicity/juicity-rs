use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use juicity_common::consts;
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

/// Single-map entry combining auth data and insertion timestamp for cache locality.
struct InFlightEntry {
    auth: UnderlayAuth,
    inserted_at: Instant,
}

struct InFlightInner {
    entries: HashMap<InFlightKey, InFlightEntry>,
}

impl InFlightUnderlayKey {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            inner: Mutex::new(InFlightInner {
                entries: HashMap::new(),
            }),
            notify: Notify::new(),
        }
    }

    /// Store an authentication for later retrieval.
    ///
    /// If the number of in-flight entries already equals `MAX_IN_FLIGHT_UNDERLAY_ENTRIES`,
    /// expired entries are evicted first.  If the map is still full after eviction,
    /// the new entry is silently dropped to prevent unbounded memory growth during a
    /// burst of forged or unanswered underlay auth packets.
    pub fn store(&self, key: InFlightKey, auth: UnderlayAuth) {
        let mut inner = self.inner.lock().unwrap();
        if inner.entries.len() >= consts::MAX_IN_FLIGHT_UNDERLAY_ENTRIES {
            // Eagerly evict expired entries before considering whether to drop.
            let now = Instant::now();
            let ttl = self.ttl;
            inner.entries.retain(|_, e| now.duration_since(e.inserted_at) <= ttl);
            if inner.entries.len() >= consts::MAX_IN_FLIGHT_UNDERLAY_ENTRIES {
                tracing::warn!(
                    "in-flight underlay auth table is full ({} entries); dropping new entry",
                    inner.entries.len()
                );
                return;
            }
        }
        inner.entries.insert(key, InFlightEntry { auth, inserted_at: Instant::now() });
        // Use notify_waiters() instead of notify_one() to prevent notification loss.
        //
        // notify_one() has a subtle semantic: if the notification is sent between the
        // waiter's first direct check and its registration via notified(), the
        // notification is lost and the waiter must fall back to the 100ms timeout.
        // Under high underlay connection concurrency this can cause cumulative delays.
        //
        // notify_waiters() wakes every task that has already called notified(), and any
        // task that calls notified() afterwards will see a "permit" (tokio's internal
        // state) immediately.  The thundering-herd concern is mitigated because:
        //   1. evict() callers are bounded by MAX_UNDERLAY_HANDLER_CONCURRENCY (1024).
        //   2. Each woken waiter immediately checks whether *its* key arrived, and if
        //      not, goes back to waiting — so most wakeups are no-ops.
        // The timeout fallback is retained as a safety net for the unlikely case where
        // the waiter misses both the direct check and the notification.
        self.notify.notify_waiters();
    }

    /// Evict and retrieve an authentication using Notify for zero-latency wakeup.
    /// Uses a loop with notified() to avoid the 100ms sleep penalty.
    pub async fn evict(&self, key: &InFlightKey) -> Option<UnderlayAuth> {
        // First attempt without waiting
        {
            let mut inner = self.inner.lock().unwrap();
            if let Some(entry) = inner.entries.remove(key) {
                return Some(entry.auth);
            }
        }

        // If not found yet, wait for notification with a short timeout
        // to handle the case where the key never arrives.
        // We use a loop to re-check after notification, since notify_one()
        // may wake a waiter whose key has not yet arrived (another waiter
        // may have already consumed the newly inserted key before us).
        let deadline = Instant::now() + Duration::from_millis(100);
        loop {
            let wait = self.notify.notified();
            tokio::select! {
                _ = wait => {
                    // Woken up - check if our key arrived
                    let mut guard = self.inner.lock().unwrap();
                    if let Some(entry) = guard.entries.remove(key) {
                        return Some(entry.auth);
                    }
                    // Not our key, loop back to wait again (if within deadline)
                    if Instant::now() >= deadline {
                        return None;
                    }
                }
                _ = tokio::time::sleep_until(deadline.into()) => {
                    // Timeout - try one last time
                    let mut guard = self.inner.lock().unwrap();
                    return guard.entries.remove(key).map(|e| e.auth);
                }
            }
        }
    }

    /// Clean up expired keys.
    pub fn cleanup(&self) {
        let mut inner = self.inner.lock().unwrap();
        let now = Instant::now();
        let ttl = self.ttl;
        inner.entries.retain(|_, e| now.duration_since(e.inserted_at) <= ttl);
    }
}
