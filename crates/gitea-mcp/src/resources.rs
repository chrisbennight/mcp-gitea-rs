//! Bounded storage for operation payloads too large to place in a context window.
//!
//! An agent asking for a job log, an artifact, or a repository archive receives
//! whatever the upstream returns. Those payloads are routinely megabytes, and
//! inlining one costs the caller its entire working context while telling it
//! almost nothing. The transport ceiling in `gitea-api` does not help: it exists
//! to keep the process alive, not to keep a response readable, so everything
//! below it is inlined whole.
//!
//! Payloads above the context-scale ceiling are stored here and referenced by
//! URI instead. The store is deliberately small and forgetful — it is a landing
//! area for one conversation's oversized reads, not a cache and not a file
//! server — so it is bounded three ways at once: per object, in aggregate, and
//! by age. Every one of those bounds can be reached by ordinary use, so each is
//! enforced rather than documented.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

/// Bytes charged for an entry regardless of its body.
///
/// A payload costs more than its bytes: the URI, operation id, media type, map
/// node, and bookkeeping all persist for as long as it does. Charging only the
/// body would let arbitrarily many empty payloads — a response displaced because
/// its envelope was large rather than its content — accumulate without ever
/// touching the aggregate cap.
const ENTRY_OVERHEAD_BYTES: usize = 512;

/// URI scheme for stored payloads. Resources are process-local and expire, so
/// the scheme is deliberately not `file:` or `http:`: a caller must come back
/// through this server to read one.
pub const RESOURCE_URI_SCHEME: &str = "gitea-response";

/// The three caps the store enforces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceLimits {
    /// Largest single payload that may be stored.
    pub max_object_bytes: usize,
    /// Largest total across all live payloads.
    pub max_total_bytes: usize,
    /// How long a payload remains readable after it is stored.
    pub time_to_live: Duration,
}

/// A payload held for later retrieval.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredResource {
    pub uri: String,
    pub operation_id: String,
    pub content_type: String,
    /// Carried from the operation's generated classification. A displaced
    /// payload is read back through a different call than the one that produced
    /// it, so without this the sensitivity of a credential-bearing response
    /// would be known only to the reply that no longer contains it.
    pub sensitive: bool,
    pub body: Vec<u8>,
}

/// Why a payload could not be stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreError {
    /// The payload exceeds what the store will ever hold, so no amount of
    /// eviction would make room. Reported rather than silently truncated.
    ObjectTooLarge { bytes: usize, limit: usize },
    /// The payload fits the caps in principle, but other live sessions are
    /// holding enough that it will not fit now. Reported rather than evicting
    /// another session's data, which this session cannot see and does not own.
    ///
    /// `required` is what retaining the payload would cost, which is its body
    /// plus the per-entry charge — not the body alone. Reporting the body would
    /// produce explanations that contradict the decision whenever free capacity
    /// falls between the two.
    BudgetExhausted { required: usize, available: usize },
}

/// Retained bytes shared by every session's store.
///
/// Each session owns its payloads, but they all occupy one process. Without a
/// shared account, N sessions would each be entitled to the full aggregate cap
/// and total retention would grow without bound in the number of sessions.
pub struct ResourceBudget {
    max_total_bytes: usize,
    used: AtomicUsize,
    /// Every live store charging this budget, weakly held so a finished session
    /// is collected normally. Used to reclaim expired charges from a store whose
    /// own session has gone idle: expiry otherwise runs only when its owner is
    /// touched, so one quiet conversation could hold capacity it can no longer
    /// read.
    stores: Mutex<Vec<Weak<ResourceStore>>>,
}

impl ResourceBudget {
    #[must_use]
    pub fn new(max_total_bytes: usize) -> Self {
        Self {
            max_total_bytes,
            used: AtomicUsize::new(0),
            stores: Mutex::new(Vec::new()),
        }
    }

    fn register(&self, store: &Arc<ResourceStore>) {
        let mut stores = self.stores.lock().unwrap_or_else(PoisonError::into_inner);
        stores.retain(|weak| weak.strong_count() > 0);
        stores.push(Arc::downgrade(store));
    }

    /// Drop expired entries from every live store, releasing their charges.
    ///
    /// Skips any store already locked by the caller: that store is mid-insert
    /// and has just expired its own entries, so waiting on it would deadlock for
    /// no gain.
    fn reclaim_expired(&self, now: Instant) {
        let stores: Vec<Arc<ResourceStore>> = {
            let mut stores = self.stores.lock().unwrap_or_else(PoisonError::into_inner);
            stores.retain(|weak| weak.strong_count() > 0);
            stores.iter().filter_map(Weak::upgrade).collect()
        };
        for store in stores {
            if let Ok(mut state) = store.state.try_lock() {
                ResourceStore::expire(&mut state, now, store.limits.time_to_live, self);
            }
        }
    }

    #[must_use]
    pub fn used_bytes(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }

    #[must_use]
    pub const fn capacity_bytes(&self) -> usize {
        self.max_total_bytes
    }

    /// Reserve space, or report what remains when it will not fit.
    fn reserve(&self, bytes: usize) -> Result<(), usize> {
        let mut current = self.used.load(Ordering::Acquire);
        loop {
            let Some(next) = current.checked_add(bytes) else {
                return Err(self.max_total_bytes.saturating_sub(current));
            };
            if next > self.max_total_bytes {
                return Err(self.max_total_bytes.saturating_sub(current));
            }
            match self.used.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(observed) => current = observed,
            }
        }
    }

    fn release(&self, bytes: usize) {
        self.used.fetch_sub(bytes, Ordering::AcqRel);
    }
}

struct Entry {
    resource: StoredResource,
    stored_at: Instant,
    sequence: u64,
}

struct State {
    entries: HashMap<String, Entry>,
    /// Entries touched while serving reads, so a test can assert that a page
    /// costs what it returns rather than what the store holds.
    #[cfg(test)]
    visits: std::cell::Cell<usize>,
    /// Insertion order, so a page can be read by range rather than by sorting
    /// every retained entry. Eviction and expiry both consult it, and a listing
    /// costs what it returns rather than what is held.
    order: BTreeMap<u64, String>,
    total_bytes: usize,
    next_sequence: u64,
}

impl State {
    /// Record that one retained entry was examined.
    ///
    /// Compiles away outside tests. It exists so a test can assert that reading
    /// a page costs what the page returns, which is invisible from outside the
    /// store — and was for that reason claimed by a test that never checked it.
    #[cfg(test)]
    fn visit(&self) {
        self.visits.set(self.visits.get() + 1);
    }

    #[cfg(not(test))]
    #[expect(
        clippy::unused_self,
        reason = "mirrors the signature of the test-only counter it replaces"
    )]
    const fn visit(&self) {}
}

/// One session's payloads, charged against a process-wide byte budget.
pub struct ResourceStore {
    limits: ResourceLimits,
    budget: Arc<ResourceBudget>,
    state: Mutex<State>,
}

impl ResourceStore {
    /// A store with a budget of its own. Used where there is only one store.
    #[must_use]
    pub fn new(limits: ResourceLimits) -> Arc<Self> {
        Self::with_budget(
            limits,
            Arc::new(ResourceBudget::new(limits.max_total_bytes)),
        )
    }

    /// A store sharing an existing process-wide budget.
    ///
    /// Returned behind an `Arc` because the budget keeps a weak handle to it, in
    /// order to reclaim expired charges from a session that has gone idle.
    #[must_use]
    pub fn with_budget(limits: ResourceLimits, budget: Arc<ResourceBudget>) -> Arc<Self> {
        let store = Arc::new(Self {
            limits,
            budget,
            state: Mutex::new(State {
                entries: HashMap::new(),
                #[cfg(test)]
                visits: std::cell::Cell::new(0),
                order: BTreeMap::new(),
                total_bytes: 0,
                next_sequence: 0,
            }),
        });
        store.budget.register(&store);
        store
    }

    #[must_use]
    pub const fn limits(&self) -> ResourceLimits {
        self.limits
    }

    /// Store a payload and return its handle.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::ObjectTooLarge`] when the payload exceeds either
    /// the per-object cap or the aggregate cap, since neither can be satisfied
    /// by evicting other entries.
    pub fn insert(
        &self,
        operation_id: &str,
        content_type: &str,
        sensitive: bool,
        body: Vec<u8>,
    ) -> Result<StoredResource, StoreError> {
        self.insert_at(Instant::now(), operation_id, content_type, sensitive, body)
    }

    /// The newest entry's age stamp, which every later one is clamped to.
    #[cfg(test)]
    fn newest_stored_at(&self) -> Option<Instant> {
        let state = self.lock();
        state
            .order
            .iter()
            .next_back()
            .and_then(|(_, uri)| state.entries.get(uri))
            .map(|entry| entry.stored_at)
    }

    /// Read a payload, or `None` when it is unknown or has expired.
    #[must_use]
    pub fn read(&self, uri: &str) -> Option<StoredResource> {
        self.read_at(Instant::now(), uri)
    }

    /// Every live payload, most recently stored first, without bodies.
    #[must_use]
    pub fn list(&self) -> Vec<StoredResource> {
        self.list_at(Instant::now())
    }

    /// [`Self::insert`] against a caller-supplied clock reading.
    ///
    /// # Errors
    ///
    /// As [`Self::insert`].
    pub fn insert_at(
        &self,
        now: Instant,
        operation_id: &str,
        content_type: &str,
        sensitive: bool,
        body: Vec<u8>,
    ) -> Result<StoredResource, StoreError> {
        // A payload larger than the whole store can never be admitted, so this
        // is reported before any eviction: dropping live entries to make room
        // for something that still will not fit loses data for nothing.
        // Judged on what the entry will actually cost, not on its body alone: a
        // body that fits the aggregate cap while its charge does not would
        // otherwise pass here, evict every live entry to make room, and then be
        // refused anyway — losing data for a payload that could never be kept.
        let charge = body.len().saturating_add(ENTRY_OVERHEAD_BYTES);
        let admissible = self.limits.max_object_bytes.min(
            self.limits
                .max_total_bytes
                .saturating_sub(ENTRY_OVERHEAD_BYTES),
        );
        if body.len() > admissible {
            return Err(StoreError::ObjectTooLarge {
                bytes: body.len(),
                limit: admissible,
            });
        }

        let mut state = self.lock();
        Self::expire(&mut state, now, self.limits.time_to_live, &self.budget);

        // Make room within this session first, so a session that keeps reading
        // large objects recycles its own space instead of consuming the shared
        // budget indefinitely.
        while state.total_bytes.saturating_add(charge) > self.limits.max_total_bytes {
            if !Self::evict_oldest(&mut state, &self.budget) {
                break;
            }
        }

        // Then charge the shared account. If other sessions are holding too
        // much, this insert is refused rather than evicting payloads belonging
        // to a session this one cannot see and does not own.
        let mut available = self.budget.reserve(charge).err();
        if available.is_some() {
            // Another session may be holding capacity for payloads that have
            // already expired but whose owner has not been touched since.
            self.budget.reclaim_expired(now);
            available = self.budget.reserve(charge).err();
        }
        while available.is_some() {
            if !Self::evict_oldest(&mut state, &self.budget) {
                break;
            }
            available = self.budget.reserve(charge).err();
        }
        if let Some(available) = available {
            return Err(StoreError::BudgetExhausted {
                required: charge,
                available,
            });
        }

        // Expiry walks the index from the front and stops at the first live
        // entry, which is only sound while sequence order and age order agree.
        // A caller's clock reading is taken before this lock, so two concurrent
        // inserts can be sequenced opposite to their timestamps and strand an
        // older expired entry behind a newer live one. Clamping forward makes
        // the ordering hold by construction rather than by timing.
        let stored_at = state
            .order
            .iter()
            .next_back()
            .and_then(|(_, uri)| state.entries.get(uri))
            .map_or(now, |newest| now.max(newest.stored_at));

        let sequence = state.next_sequence;
        state.next_sequence += 1;
        let uri = format!("{RESOURCE_URI_SCHEME}:/{operation_id}/{sequence}");
        let resource = StoredResource {
            uri: uri.clone(),
            operation_id: operation_id.to_string(),
            content_type: content_type.to_string(),
            sensitive,
            body,
        };
        state.total_bytes = state
            .total_bytes
            .saturating_add(resource.body.len().saturating_add(ENTRY_OVERHEAD_BYTES));
        state.order.insert(sequence, uri.clone());
        state.entries.insert(
            uri,
            Entry {
                resource: resource.clone(),
                stored_at,
                sequence,
            },
        );
        Ok(resource)
    }

    /// [`Self::read`] against a caller-supplied clock reading.
    #[must_use]
    pub fn read_at(&self, now: Instant, uri: &str) -> Option<StoredResource> {
        let mut state = self.lock();
        Self::expire(&mut state, now, self.limits.time_to_live, &self.budget);
        state.entries.get(uri).map(|entry| entry.resource.clone())
    }

    /// One page of live payloads, without bodies, resumed after `cursor`.
    ///
    /// Paging is done under the lock rather than by materialising the whole
    /// store and slicing it: a listing should cost what it returns, not what is
    /// retained.
    #[must_use]
    pub fn page(
        &self,
        cursor: Option<&str>,
        limit: usize,
    ) -> (Vec<StoredResource>, Option<String>) {
        self.page_at(Instant::now(), cursor, limit)
    }

    /// [`Self::page`] against a caller-supplied clock reading.
    #[must_use]
    pub fn page_at(
        &self,
        now: Instant,
        cursor: Option<&str>,
        limit: usize,
    ) -> (Vec<StoredResource>, Option<String>) {
        let mut state = self.lock();
        Self::expire(&mut state, now, self.limits.time_to_live, &self.budget);

        // Read by range over the insertion index, newest first. Sorting every
        // retained entry would make a fifty-item page cost what the whole store
        // holds, which is the opposite of what paging is for.
        let upper = cursor
            .and_then(|cursor| state.entries.get(cursor))
            .map(|entry| entry.sequence);
        let mut page = Vec::with_capacity(limit.min(state.order.len()));
        let mut next = None;
        let candidates: Vec<String> = match upper {
            Some(sequence) => state
                .order
                .range(..sequence)
                .rev()
                .take(limit + 1)
                .map(|(_, uri)| uri.clone())
                .collect(),
            None => state
                .order
                .iter()
                .rev()
                .take(limit + 1)
                .map(|(_, uri)| uri.clone())
                .collect(),
        };
        for uri in candidates {
            state.visit();
            if page.len() == limit {
                // One entry past the page proves more remains without reading it.
                next = page
                    .last()
                    .map(|stored: &StoredResource| stored.uri.clone());
                break;
            }
            if let Some(entry) = state.entries.get(&uri) {
                // Built field by field: cloning the resource first would copy a
                // body that a listing exists to omit.
                page.push(StoredResource {
                    uri: entry.resource.uri.clone(),
                    operation_id: entry.resource.operation_id.clone(),
                    content_type: entry.resource.content_type.clone(),
                    sensitive: entry.resource.sensitive,
                    body: Vec::new(),
                });
            }
        }
        (page, next)
    }

    /// [`Self::list`] against a caller-supplied clock reading.
    #[must_use]
    pub fn list_at(&self, now: Instant) -> Vec<StoredResource> {
        let mut state = self.lock();
        Self::expire(&mut state, now, self.limits.time_to_live, &self.budget);
        let mut entries: Vec<_> = state.entries.values().collect();
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.sequence));
        entries
            .into_iter()
            .map(|entry| StoredResource {
                body: Vec::new(),
                ..entry.resource.clone()
            })
            .collect()
    }

    /// Live entries and index entries, which must always agree.
    #[cfg(test)]
    fn index_sizes(&self) -> (usize, usize) {
        let state = self.lock();
        (state.entries.len(), state.order.len())
    }

    /// Entries touched since the counter was last reset.
    #[cfg(test)]
    fn take_visits(&self) -> usize {
        let state = self.lock();
        let seen = state.visits.get();
        state.visits.set(0);
        seen
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // A panic while holding the lock leaves the map consistent — every
        // mutation below completes before the guard drops — so recovering is
        // preferable to propagating an unrelated panic into every later call.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Drop entries whose lifetime has elapsed, visiting only those that have.
    ///
    /// Every payload has the same lifetime, so storage order is expiry order and
    /// the expired ones are always a prefix of the index. Walking from the front
    /// and stopping at the first live entry costs what it removes; scanning
    /// every entry to find them made a fifty-item page pay for the whole store.
    fn expire(state: &mut State, now: Instant, time_to_live: Duration, budget: &ResourceBudget) {
        while let Some((sequence, uri)) = state
            .order
            .iter()
            .next()
            .map(|(sequence, uri)| (*sequence, uri.clone()))
        {
            state.visit();
            let Some(entry) = state.entries.get(&uri) else {
                // Index and map disagree; prune rather than stall.
                state.order.remove(&sequence);
                continue;
            };
            if now.duration_since(entry.stored_at) < time_to_live {
                break;
            }
            Self::remove(state, &uri, budget);
        }
    }

    /// Drop the oldest entry, reporting whether anything was dropped.
    ///
    /// Every call makes progress: a sequence present in the index but missing
    /// from the map is pruned rather than retried. Without that, any divergence
    /// between the two would spin the eviction loops forever instead of
    /// surfacing as a wrong answer.
    fn evict_oldest(state: &mut State, budget: &ResourceBudget) -> bool {
        let Some((sequence, uri)) = state
            .order
            .iter()
            .next()
            .map(|(sequence, uri)| (*sequence, uri.clone()))
        else {
            return false;
        };
        if state.entries.contains_key(&uri) {
            Self::remove(state, &uri, budget);
        } else {
            state.order.remove(&sequence);
        }
        true
    }

    fn remove(state: &mut State, uri: &str, budget: &ResourceBudget) {
        if let Some(entry) = state.entries.remove(uri) {
            state.order.remove(&entry.sequence);
            let charge = entry
                .resource
                .body
                .len()
                .saturating_add(ENTRY_OVERHEAD_BYTES);
            state.total_bytes = state.total_bytes.saturating_sub(charge);
            budget.release(charge);
        }
    }
}

impl Drop for ResourceStore {
    fn drop(&mut self) {
        // A finished session frees its payload bodies with the map, but the
        // shared account only knows what it was told. Without this, ordinary
        // session churn would charge the budget permanently and later sessions
        // would be refused with nothing actually stored.
        let state = self.state.get_mut().unwrap_or_else(PoisonError::into_inner);
        self.budget.release(state.total_bytes);
        state.total_bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What one payload of `body` bytes actually costs the store.
    fn charge(body: usize) -> usize {
        body + ENTRY_OVERHEAD_BYTES
    }

    fn store(max_object_bytes: usize, max_total_bytes: usize, seconds: u64) -> Arc<ResourceStore> {
        ResourceStore::new(ResourceLimits {
            max_object_bytes,
            max_total_bytes,
            time_to_live: Duration::from_secs(seconds),
        })
    }

    #[test]
    fn a_finished_session_returns_its_bytes_to_the_shared_budget() {
        // Dropping a store frees its payload bodies, but the shared account only
        // knows what it is told. Without drop-time accounting, ordinary session
        // churn would charge the budget permanently and later sessions would be
        // refused with nothing actually stored.
        let budget = Arc::new(ResourceBudget::new(charge(600)));
        let limits = ResourceLimits {
            max_object_bytes: 1000,
            max_total_bytes: charge(600),
            time_to_live: Duration::from_secs(90),
        };
        {
            let session = ResourceStore::with_budget(limits, Arc::clone(&budget));
            session
                .insert("a", "text/plain", false, vec![b'a'; 600])
                .unwrap();
            assert_eq!(budget.used_bytes(), charge(600));
        }
        assert_eq!(
            budget.used_bytes(),
            0,
            "a finished session must not hold capacity forever"
        );

        let next = ResourceStore::with_budget(limits, Arc::clone(&budget));
        assert!(
            next.insert("b", "text/plain", false, vec![b'b'; 600])
                .is_ok(),
            "the reclaimed capacity is usable by the next session"
        );
    }

    #[test]
    fn an_idle_sessions_expired_payloads_do_not_hold_shared_capacity() {
        // Expiry runs when a store is touched. A session that stops asking for
        // anything would otherwise keep charging the budget for payloads nobody
        // can read any more.
        let budget = Arc::new(ResourceBudget::new(charge(600)));
        let limits = ResourceLimits {
            max_object_bytes: 1000,
            max_total_bytes: charge(600),
            time_to_live: Duration::from_secs(90),
        };
        let idle = ResourceStore::with_budget(limits, Arc::clone(&budget));
        let busy = ResourceStore::with_budget(limits, Arc::clone(&budget));

        let start = Instant::now();
        idle.insert_at(start, "idle", "text/plain", false, vec![b'i'; 600])
            .unwrap();
        assert_eq!(budget.used_bytes(), charge(600));

        // The idle session is never touched again; the busy one asks for space
        // after the idle payload's lifetime has elapsed.
        let later = start + Duration::from_secs(91);
        let stored = busy
            .insert_at(later, "busy", "text/plain", false, vec![b'b'; 600])
            .expect("expired capacity is reclaimed from the idle session");

        assert!(busy.read_at(later, &stored.uri).is_some());
        assert!(idle.read_at(later, "gitea-response:/idle/0").is_none());
    }

    #[test]
    fn the_ordering_index_never_outgrows_the_entries_it_indexes() {
        // The index and the map are two views of one set. Eviction repairs a
        // divergence it happens to walk past, and paging reads newest-first so
        // stale old sequences rarely surface — which means a leak here would
        // never show up as a wrong answer, only as memory that never comes back.
        let store = store(4096, charge(8) * 4, 90);
        let start = Instant::now();
        for index in 0..30 {
            store
                .insert_at(
                    start,
                    &format!("op{index}"),
                    "text/plain",
                    false,
                    vec![b'x'; 8],
                )
                .expect("stored");
        }
        let (entries, indexed) = store.index_sizes();
        assert_eq!(entries, indexed, "eviction keeps the two in step");

        // Expiry removes entries through the same path.
        let later = start + Duration::from_secs(91);
        store
            .insert_at(later, "fresh", "text/plain", false, vec![b'f'; 8])
            .expect("stored");
        let (entries, indexed) = store.index_sizes();
        assert_eq!(entries, indexed, "expiry keeps the two in step");
        assert_eq!(entries, 1, "only the fresh payload survives");
    }

    #[test]
    fn eviction_leaves_no_phantom_entries_in_a_page() {
        // A page reads candidates from the ordering index and skips any whose
        // entry is gone. If eviction left the index untouched, those gaps would
        // silently shorten every page: the store would report fewer payloads
        // than it holds, and a cursor walk would stop early.
        let store = store(4096, charge(8) * 4, 3600);
        for index in 0..40 {
            store
                .insert(&format!("op{index}"), "text/plain", false, vec![b'x'; 8])
                .expect("stored");
        }

        let live = store.list().len();
        assert!(live > 0 && live <= 4, "the cap forced evictions");

        let (page, next) = store.page(None, 50);
        assert_eq!(
            page.len(),
            live,
            "a page must return every live entry, not the survivors of a stale index"
        );
        assert!(next.is_none());
    }

    #[test]
    fn a_page_costs_what_it_returns_not_what_is_held() {
        // Asserted by counting entries touched, not by inspecting the page. An
        // earlier version of this test checked only length, omitted bodies, and
        // non-overlap, all of which held while the expiry pass still scanned
        // every retained entry on the way in — so it passed against exactly the
        // cost it was named for.
        let held = 2000;
        let store = store(4096, charge(8) * (held + 10), 3600);
        for index in 0..held {
            store
                .insert(&format!("op{index}"), "text/plain", false, vec![b'x'; 8])
                .expect("stored");
        }
        store.take_visits();

        let (page, cursor) = store.page(None, 50);
        let visited = store.take_visits();

        assert_eq!(page.len(), 50);
        assert!(page.iter().all(|stored| stored.body.is_empty()));
        assert!(
            visited <= 60,
            "a fifty-entry page touched {visited} of {held} retained entries"
        );

        // And the same holds part-way through a walk, not only at its start.
        let (second, _) = store.page(cursor.as_deref(), 50);
        let visited = store.take_visits();
        assert_eq!(second.len(), 50);
        assert!(
            visited <= 60,
            "a resumed page touched {visited} of {held} retained entries"
        );
    }

    #[test]
    fn an_out_of_order_clock_reading_cannot_strand_an_expired_entry() {
        // Reproduces the concurrency hazard deterministically: a caller's clock
        // reading is taken before the store lock, so two racing inserts can be
        // sequenced opposite to their timestamps. Expiry walks the index from
        // the front and stops at the first live entry, so an older expired entry
        // sequenced behind a newer live one would stay readable and charged past
        // its lifetime.
        let store = store(4096, charge(8) * 10, 90);
        let start = Instant::now();

        // Sequenced second but stamped earlier, as a lost race would produce.
        store
            .insert_at(
                start + Duration::from_secs(50),
                "newer",
                "text/plain",
                false,
                vec![b'n'; 8],
            )
            .expect("stored");
        store
            .insert_at(start, "older", "text/plain", false, vec![b'o'; 8])
            .expect("stored");

        // Clamping means the later-sequenced entry is never treated as older.
        let newest = store.newest_stored_at().expect("an entry");
        assert!(
            newest >= start + Duration::from_secs(50),
            "age order must follow sequence order"
        );

        // At a moment past the first entry's lifetime but before the second's,
        // the walk must not stop early and leave the expired one behind.
        let probe = start + Duration::from_secs(141);
        assert_eq!(
            store.page_at(probe, None, 50).0.len(),
            0,
            "everything past its lifetime is gone, whatever order it arrived in"
        );
        let (entries, indexed) = store.index_sizes();
        assert_eq!(entries, 0);
        assert_eq!(indexed, 0);
    }

    #[test]
    fn expiry_visits_only_what_has_expired() {
        // Uniform lifetimes make storage order expiry order, so the expired
        // entries are a prefix. Stopping at the first live one is what keeps a
        // read proportional to what it removes.
        let store = store(4096, charge(8) * 3000, 90);
        let start = Instant::now();
        for index in 0..1000 {
            store
                .insert_at(
                    start,
                    &format!("old{index}"),
                    "text/plain",
                    false,
                    vec![b'x'; 8],
                )
                .expect("stored");
        }
        let later = start + Duration::from_secs(91);
        for index in 0..5 {
            store
                .insert_at(
                    later,
                    &format!("new{index}"),
                    "text/plain",
                    false,
                    vec![b'n'; 8],
                )
                .expect("stored");
        }
        store.take_visits();

        // Everything old is already gone; a further read must not re-walk it.
        let (page, _) = store.page_at(later, None, 50);
        let visited = store.take_visits();
        assert_eq!(page.len(), 5, "only the fresh payloads remain");
        assert!(
            visited <= 15,
            "a read after expiry touched {visited} entries"
        );
    }

    #[test]
    fn exhaustion_reports_what_retention_would_actually_cost() {
        // Reporting the body alone produced explanations that contradicted the
        // decision whenever free capacity fell between body and body+overhead.
        let budget = Arc::new(ResourceBudget::new(charge(100)));
        let limits = ResourceLimits {
            max_object_bytes: 4096,
            max_total_bytes: charge(100),
            time_to_live: Duration::from_secs(90),
        };
        let held = ResourceStore::with_budget(limits, Arc::clone(&budget));
        let other = ResourceStore::with_budget(limits, Arc::clone(&budget));
        held.insert("held", "text/plain", false, vec![b'h'; 100])
            .expect("stored");

        let refused = other
            .insert("blocked", "text/plain", false, vec![b'b'; 100])
            .expect_err("the shared account is full");

        match refused {
            StoreError::BudgetExhausted {
                required,
                available,
            } => {
                assert_eq!(required, charge(100), "the entry, not just its body");
                assert!(
                    required > available,
                    "the reported numbers must justify the refusal"
                );
            }
            other @ StoreError::ObjectTooLarge { .. } => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn what_is_reserved_is_what_is_released() {
        // Every reservation must charge what the eventual release returns. When
        // the eviction retry reserved a body while release returned body plus
        // overhead, the account drifted on each replacement and could wrap,
        // after which nothing could be stored again.
        //
        // Two sessions are required to reach that retry: a single store evicts
        // its own entries down to its per-session cap before the shared
        // reservation can fail, so the retry never runs.
        let body = 200;
        let budget = Arc::new(ResourceBudget::new(charge(body) * 2));
        let limits = ResourceLimits {
            max_object_bytes: 4096,
            max_total_bytes: charge(body) * 2,
            time_to_live: Duration::from_secs(90),
        };
        let held = ResourceStore::with_budget(limits, Arc::clone(&budget));
        let busy = ResourceStore::with_budget(limits, Arc::clone(&budget));

        // One session parks a payload, so the shared account is half consumed
        // and the other session's reservations must go through the retry.
        held.insert("held", "text/plain", false, vec![b'h'; body])
            .expect("stored");

        for index in 0..10 {
            busy.insert(&format!("op{index}"), "text/plain", false, vec![b'b'; body])
                .expect("the busy session recycles its own space");
        }

        let live = held.list().len() + busy.list().len();
        assert_eq!(
            budget.used_bytes(),
            live * charge(body),
            "the shared account must equal what is actually held"
        );
        assert!(
            budget.used_bytes() <= budget.capacity_bytes(),
            "and must never exceed the capacity it is meant to bound"
        );
    }

    #[test]
    fn an_impossible_payload_is_refused_before_anything_is_evicted() {
        // Admissibility is judged on the entry's real cost. Judging it on the
        // body alone let a payload that could never be kept evict every live
        // entry first.
        // 1200 bytes is under the aggregate cap of charge(1000) = 1512, so the
        // old body-only check admitted it; its real cost of 1712 never could.
        let store = store(4096, charge(1000), 60);
        let keeper = store
            .insert("keeper", "text/plain", false, vec![b'k'; 100])
            .expect("stored");

        let refused = store.insert("huge", "text/plain", false, vec![b'h'; 1200]);

        assert!(matches!(refused, Err(StoreError::ObjectTooLarge { .. })));
        assert!(
            store.read(&keeper.uri).is_some(),
            "a payload that can never fit must not cost live entries"
        );
    }

    #[test]
    fn empty_payloads_cannot_accumulate_without_bound() {
        // A response displaced for the size of its envelope rather than its
        // body still costs a URI, an operation id, a media type, and a map node.
        // Charging only the body would let arbitrarily many of those accumulate
        // while the aggregate cap reported room to spare.
        let store = store(4096, charge(0) * 3, 60);
        for index in 0..10 {
            store
                .insert(&format!("op{index}"), "text/plain", false, Vec::new())
                .expect("an empty payload is still storable");
        }
        assert_eq!(
            store.list().len(),
            3,
            "the aggregate cap bounds how many entries can be live, not only their bytes"
        );
    }

    #[test]
    fn rejects_a_payload_larger_than_the_per_object_cap() {
        // Refused rather than truncated: a caller that receives half a job log
        // with no indication has been given wrong data, not less data.
        let store = store(64, 1024, 60);
        assert_eq!(
            store.insert("repoDownload", "text/plain", false, vec![b'x'; 65]),
            Err(StoreError::ObjectTooLarge {
                bytes: 65,
                limit: 64
            })
        );
    }

    #[test]
    fn evicts_oldest_payloads_to_honour_the_aggregate_cap() {
        let store = store(1024, charge(60) * 2 - 1, 60);
        let first = store
            .insert("a", "text/plain", false, vec![b'a'; 60])
            .unwrap();
        let second = store
            .insert("b", "text/plain", false, vec![b'b'; 60])
            .unwrap();

        // The second payload does not fit beside the first, so the first goes.
        assert!(store.read(&first.uri).is_none());
        assert_eq!(
            store.read(&second.uri).map(|stored| stored.body.len()),
            Some(60)
        );
    }

    #[test]
    fn space_reclaimed_by_eviction_is_usable_again() {
        // Guards the bookkeeping rather than the eviction. The leak only shows
        // after something has been evicted: if removal did not release the
        // bytes it held, the running total would stay high forever and the
        // store would evict everything on every later insert, so a small
        // payload could never sit beside the one that displaced its
        // predecessor.
        let store = store(1024, charge(60) * 2 - 1, 60);
        store
            .insert("a", "text/plain", false, vec![b'a'; 60])
            .unwrap();
        let survivor = store
            .insert("b", "text/plain", false, vec![b'b'; 60])
            .unwrap();
        assert_eq!(store.list().len(), 1, "the first payload was evicted");

        let small = store
            .insert("c", "text/plain", false, vec![b'c'; 30])
            .unwrap();

        assert!(
            store.read(&survivor.uri).is_some(),
            "60 + 30 fits within the cap, so nothing more should be evicted"
        );
        assert!(store.read(&small.uri).is_some());
        assert_eq!(store.list().len(), 2);
    }

    #[test]
    fn aggregate_cap_counts_evictions_rather_than_accumulating() {
        // A store that subtracted nothing on eviction would refuse everything
        // after enough churn, while still holding almost nothing.
        let store = store(1024, charge(60) * 2 - 1, 60);
        let mut latest = None;
        for _ in 0..20 {
            latest = Some(
                store
                    .insert("a", "text/plain", false, vec![b'a'; 60])
                    .unwrap(),
            );
        }
        let latest = latest.unwrap();
        assert_eq!(
            store.read(&latest.uri).map(|stored| stored.body.len()),
            Some(60)
        );
        assert_eq!(store.list().len(), 1);
    }

    #[test]
    fn a_payload_is_unreadable_once_its_lifetime_elapses() {
        // Driven by an injected clock reading rather than by sleeping, so the
        // boundary is exercised exactly rather than approximately.
        let store = store(1024, charge(1024) * 4, 90);
        let start = Instant::now();
        let stored = store
            .insert_at(start, "repoDownload", "text/plain", false, vec![b'x'; 10])
            .unwrap();

        assert!(
            store
                .read_at(start + Duration::from_secs(89), &stored.uri)
                .is_some(),
            "readable right up to its lifetime"
        );
        assert!(
            store
                .read_at(start + Duration::from_secs(90), &stored.uri)
                .is_none(),
            "gone the instant its lifetime elapses"
        );
    }

    #[test]
    fn expiry_releases_the_space_it_was_occupying() {
        let store = store(1024, charge(60) * 2 - 1, 60);
        let start = Instant::now();
        let first = store
            .insert_at(start, "a", "text/plain", false, vec![b'a'; 90])
            .unwrap();
        let later = start + Duration::from_secs(61);
        let second = store
            .insert_at(later, "b", "text/plain", false, vec![b'b'; 90])
            .unwrap();

        assert!(store.read_at(later, &first.uri).is_none());
        assert!(store.read_at(later, &second.uri).is_some());
    }

    #[test]
    fn listing_omits_bodies_but_keeps_identity() {
        // The list is a catalogue, not a delivery mechanism; returning bodies
        // here would reintroduce exactly the flood the store exists to prevent.
        let store = store(1024, charge(1024) * 4, 60);
        store
            .insert("a", "text/plain", false, vec![b'a'; 40])
            .unwrap();
        let listed = store.list();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].body.is_empty());
        assert_eq!(listed[0].operation_id, "a");
        assert_eq!(listed[0].content_type, "text/plain");
    }

    #[test]
    fn distinct_payloads_from_one_operation_get_distinct_uris() {
        let store = store(1024, charge(1024) * 4, 60);
        let first = store
            .insert("a", "text/plain", false, vec![b'1'; 10])
            .unwrap();
        let second = store
            .insert("a", "text/plain", false, vec![b'2'; 10])
            .unwrap();
        assert_ne!(first.uri, second.uri);
        assert_eq!(store.read(&first.uri).map(|s| s.body), Some(vec![b'1'; 10]));
        assert_eq!(
            store.read(&second.uri).map(|s| s.body),
            Some(vec![b'2'; 10])
        );
    }
}
