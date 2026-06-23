use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use juicity_common::consts;
use juicity_common::protocol::UnderlayAuth;
use tokio::sync::Notify;

/// In-flight key type (32 bytes salt)
pub type InFlightKey = [u8; 32];

/// Manages underlay authentication keys that are in-flight (waiting for their
/// corresponding UDP packets).
///
/// Each key has its own [`Notify`] so that [`store`](Self::store) only wakes
/// tasks waiting for the exact key that was inserted — avoiding the thundering
/// herd problem of a single shared Notify.
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
/// loop, but the per-key Notify is cloned **before** the `.await`, so the lock is released
/// before entering the wait state.  This makes it safe.
pub struct InFlightUnderlayKey {
    ttl: Duration,
    evict_timeout: Duration,
    inner: Mutex<InFlightInner>,
}

/// Single-map entry combining auth data, insertion timestamp and a per-key
/// [`Notify`] for cache locality.
struct InFlightEntry {
    auth: Option<UnderlayAuth>,
    inserted_at: Instant,
    notify: Arc<Notify>,
}

struct InFlightInner {
    entries: HashMap<InFlightKey, InFlightEntry>,
}

impl InFlightUnderlayKey {
    /// Create a new `InFlightUnderlayKey` with the given TTL and evict timeout.
    pub fn new(ttl: Duration, evict_timeout: Duration) -> Self {
        Self {
            ttl,
            evict_timeout,
            inner: Mutex::new(InFlightInner {
                entries: HashMap::new(),
            }),
        }
    }

    /// Store an authentication for later retrieval.
    ///
    /// If a task is already waiting for this key (via `evict`), its per-key
    /// [`Notify`] is fired so it can wake up and consume the value immediately.
    ///
    /// If the number of in-flight entries already equals
    /// `MAX_IN_FLIGHT_UNDERLAY_ENTRIES`, expired entries are evicted first.
    /// If the map is still full after eviction, the new entry is silently
    /// dropped to prevent unbounded memory growth during a burst of forged or
    /// unanswered underlay auth packets.
    pub fn store(&self, key: InFlightKey, auth: UnderlayAuth) {
        let mut inner = self.inner.lock().unwrap();

        // Key already exists — a task is waiting in evict() for it.
        if let Some(entry) = inner.entries.get_mut(&key) {
            entry.auth = Some(auth);
            // Notify **only** the tasks waiting for this specific key.
            entry.notify.notify_waiters();
            return;
        }

        // New entry: enforce capacity limit.
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

        inner.entries.insert(
            key,
            InFlightEntry {
                notify: Arc::new(Notify::new()),
                auth: Some(auth),
                inserted_at: Instant::now(),
            },
        );
    }

    /// Evict and retrieve an authentication using a per-key [`Notify`] for
    /// zero-latency wakeup.
    ///
    /// If the key is already present, it is removed and returned immediately.
    /// Otherwise a placeholder entry with a dedicated [`Notify`] is inserted,
    /// and the caller waits for that Notify.  When [`store`](Self::store)
    /// eventually fills in the value, only the task(s) waiting for this
    /// *exact* key are woken — eliminating the thundering herd.
    pub async fn evict(&self, key: &InFlightKey) -> Option<UnderlayAuth> {
        // Obtain (or create) the per-key Notify while holding the lock, then
        // drop the lock before any `.await` to uphold the safety invariant.
        let notify = {
            let mut inner = self.inner.lock().unwrap();
            match inner.entries.get_mut(key) {
                Some(entry) => {
                    if let Some(auth) = entry.auth.take() {
                        // Value already present — consume immediately.
                        inner.entries.remove(key);
                        return Some(auth);
                    }
                    // Entry exists but value not yet stored — clone its Notify
                    // and wait (the lock is released when this block ends).
                    entry.notify.clone()
                }
                None => {
                    // No entry yet — create a placeholder and wait.
                    let notify = Arc::new(Notify::new());
                    inner.entries.insert(
                        *key,
                        InFlightEntry {
                            notify: notify.clone(),
                            auth: None,
                            inserted_at: Instant::now(),
                        },
                    );
                    notify
                }
            }
        };

        // Wait for notification with a short timeout to handle keys that are
        // never stored (e.g. a forged salt that no corresponding UDP packet
        // will complete).
        let deadline = Instant::now() + self.evict_timeout;
        loop {
            tokio::select! {
                _ = notify.notified() => {
                    let mut inner = self.inner.lock().unwrap();
                    if let Some(entry) = inner.entries.remove(key) {
                        if let Some(auth) = entry.auth {
                            return Some(auth);
                        }
                        // Notified but no value — shouldn't happen under
                        // normal operation; treat as timeout.
                    }
                    if Instant::now() >= deadline {
                        return None;
                    }
                }
                _ = tokio::time::sleep_until(deadline.into()) => {
                    let mut inner = self.inner.lock().unwrap();
                    if let Some(entry) = inner.entries.remove(key) {
                        return entry.auth;
                    }
                    return None;
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
