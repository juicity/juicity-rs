use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
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
/// # Lock-free design
///
/// This implementation uses [`DashMap`] internally instead of a global
/// `std::sync::Mutex`.  DashMap provides fine-grained shard-level locking,
/// so concurrent accesses to different keys do not contend with each other.
/// This eliminates the global Mutex bottleneck under high concurrency while
/// remaining safe to use across `.await` points (no lock is held during
/// async suspension).
pub struct InFlightUnderlayKey {
    ttl: Duration,
    evict_timeout: Duration,
    /// Per-key map protected by DashMap's internal shard locks.
    /// No separate wrapper struct is needed — DashMap provides atomic
    /// entry-level operations out of the box.
    map: DashMap<InFlightKey, InFlightEntry>,
}

/// Single-map entry combining auth data, insertion timestamp and a per-key
/// [`Notify`] for cache locality.
struct InFlightEntry {
    auth: Option<UnderlayAuth>,
    inserted_at: Instant,
    notify: Arc<Notify>,
}

impl InFlightUnderlayKey {
    /// Create a new `InFlightUnderlayKey` with the given TTL and evict timeout.
    pub fn new(ttl: Duration, evict_timeout: Duration) -> Self {
        Self {
            ttl,
            evict_timeout,
            map: DashMap::new(),
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
    ///
    /// # Lock-free behaviour
    ///
    /// This method only acquires a DashMap shard lock for the target key
    /// (and briefly the entire map during capacity-based eviction).
    /// Other keys remain fully accessible to concurrent `store`/`evict` calls.
    pub fn store(&self, key: InFlightKey, auth: UnderlayAuth) {
        // Fast path: key already exists — atomically update and notify.
        if let Some(mut entry) = self.map.get_mut(&key) {
            entry.auth = Some(auth);
            entry.notify.notify_waiters();
            return;
        }

        // New entry: enforce capacity limit (best-effort approximate check,
        // since DashMap::len() is an estimated count across shards).
        if self.map.len() >= consts::MAX_IN_FLIGHT_UNDERLAY_ENTRIES {
            // Eagerly evict expired entries before considering whether to drop.
            let now = Instant::now();
            let ttl = self.ttl;
            self.map.retain(|_, e| now.duration_since(e.inserted_at) <= ttl);
            if self.map.len() >= consts::MAX_IN_FLIGHT_UNDERLAY_ENTRIES {
                tracing::warn!(
                    "in-flight underlay auth table is full ({} entries); dropping new entry",
                    self.map.len()
                );
                return;
            }
        }

        // Insert the new entry.  If another task inserted the same key between
        // our get_mut check and here, the entry API ensures we still update
        // and notify the waiter — no TOCTOU race.
        use dashmap::mapref::entry::Entry;
        match self.map.entry(key) {
            Entry::Occupied(mut entry) => {
                entry.get_mut().auth = Some(auth);
                entry.get().notify.notify_waiters();
            }
            Entry::Vacant(entry) => {
                entry.insert(InFlightEntry {
                    notify: Arc::new(Notify::new()),
                    auth: Some(auth),
                    inserted_at: Instant::now(),
                });
            }
        }
    }

    /// Evict and retrieve an authentication using a per-key [`Notify`] for
    /// zero-latency wakeup.
    ///
    /// If the key is already present, it is removed and returned immediately.
    /// Otherwise a placeholder entry with a dedicated [`Notify`] is inserted,
    /// and the caller waits for that Notify.  When [`store`](Self::store)
    /// eventually fills in the value, only the task(s) waiting for this
    /// *exact* key are woken — eliminating the thundering herd.
    ///
    /// # Lock-free behaviour
    ///
    /// No lock is held across `.await` points.  The per-key [`Notify`] is
    /// cloned before entering the async wait loop, and DashMap shard locks
    /// are only briefly held for the initial lookup (and subsequent re-checks
    /// after notification).
    pub async fn evict(&self, key: &InFlightKey) -> Option<UnderlayAuth> {
        // Obtain (or create) the per-key Notify while holding the shard lock,
        // then drop the lock before any `.await` to uphold the safety invariant.
        let notify = {
            match self.map.get_mut(key) {
                Some(mut entry) => {
                    if let Some(auth) = entry.auth.take() {
                        // Value already present — consume immediately.
                        drop(entry);
                        self.map.remove(key);
                        return Some(auth);
                    }
                    // Entry exists but value not yet stored — clone its Notify
                    // and wait (the shard lock is released when the guard drops).
                    entry.notify.clone()
                }
                None => {
                    // No entry yet — create a placeholder and wait.
                    let notify = Arc::new(Notify::new());
                    self.map.insert(
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
                    if let Some(mut entry) = self.map.get_mut(key) {
                        if let Some(auth) = entry.auth.take() {
                            drop(entry);
                            self.map.remove(key);
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
                    if let Some((_, entry)) = self.map.remove(key) {
                        return entry.auth;
                    }
                    return None;
                }
            }
        }
    }

    /// Clean up expired in-flight underlay auth entries.
    ///
    /// Iterates all shards of the internal [`DashMap`] and removes entries
    /// whose TTL (time-to-live) has elapsed since insertion.  This is
    /// intended to be run as a **background cleanup task** that holds
    /// DashMap internal shard locks only briefly per entry, so concurrent
    /// [`store`](Self::store) / [`evict`](Self::evict) operations on
    /// unrelated keys are not blocked.
    ///
    /// # Lock behaviour
    ///
    /// The lock granularity is per-DashMap-shard, and each shard is locked
    /// only for the duration of a single entry check-and-remove.  This
    /// means a full sweep across all entries has negligible impact on
    /// concurrent access to unrelated keys.
    ///
    /// # Panics
    ///
    /// This method does not panic under normal operation.  The DashMap
    /// internal locking is infallible.
    pub fn cleanup(&self) {
        let now = Instant::now();
        let ttl = self.ttl;
        self.map.retain(|_, e| now.duration_since(e.inserted_at) <= ttl);
    }
}
