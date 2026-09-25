//! Retained payload ownership is independent of transport sessions.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, PoisonError},
};

use crate::resources::{ResourceBudget, ResourceLimits, ResourceStore};

const MAX_OWNERS: usize = 256;

/// Identity established by the HTTP authentication boundary, never tool input.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ResourceOwner {
    issuer: String,
    subject: String,
}

impl ResourceOwner {
    /// Construct a verified gateway identity with bounded metadata.
    #[must_use]
    pub fn verified(issuer: &str, subject: &str) -> Option<Self> {
        let valid = |value: &str| {
            !value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
        };
        (valid(issuer) && valid(subject)).then(|| Self {
            issuer: issuer.to_owned(),
            subject: subject.to_owned(),
        })
    }
}

pub(crate) struct OwnerStores {
    stores: Mutex<BTreeMap<ResourceOwner, Arc<ResourceStore>>>,
    limits: ResourceLimits,
    budget: Arc<ResourceBudget>,
}

impl OwnerStores {
    pub(crate) fn new(limits: ResourceLimits, budget: Arc<ResourceBudget>) -> Self {
        Self {
            stores: Mutex::new(BTreeMap::new()),
            limits,
            budget,
        }
    }

    pub(crate) fn get(&self, owner: ResourceOwner) -> Option<Arc<ResourceStore>> {
        let mut stores = self.stores.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(store) = stores.get(&owner) {
            return Some(Arc::clone(store));
        }
        // Do not remove a store while an executing request could still insert
        // into it. Idle empty stores, including expired entries, are disposable.
        stores.retain(|_, store| Arc::strong_count(store) > 1 || !store.page(None, 1).0.is_empty());
        if stores.len() >= MAX_OWNERS {
            return None;
        }
        let store = ResourceStore::with_budget(self.limits, Arc::clone(&self.budget));
        stores.insert(owner, Arc::clone(&store));
        Some(store)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_recreation_does_not_reuse_a_reference() {
        let limits = ResourceLimits {
            max_object_bytes: 4096,
            max_total_bytes: 8192,
            time_to_live: std::time::Duration::ZERO,
        };
        let owners = OwnerStores::new(limits, Arc::new(ResourceBudget::new(8192)));
        let owner = ResourceOwner::verified("issuer", "alice").unwrap();
        let first = owners
            .get(owner.clone())
            .unwrap()
            .insert("fixture", "text/plain", false, b"first".to_vec())
            .unwrap()
            .uri;
        let other = owners
            .get(ResourceOwner::verified("issuer", "bob").unwrap())
            .unwrap();
        let second = owners.get(owner).unwrap();
        let replacement = second
            .insert("fixture", "text/plain", false, b"second".to_vec())
            .unwrap()
            .uri;
        assert_ne!(first, replacement);
        assert!(second.read(&first).is_none());
        drop(other);
    }

    #[test]
    fn live_owner_metadata_is_bounded_and_active_empty_stores_are_preserved() {
        let limits = ResourceLimits {
            max_object_bytes: 4096,
            max_total_bytes: 8192,
            time_to_live: std::time::Duration::from_mins(1),
        };
        let owners = OwnerStores::new(limits, Arc::new(ResourceBudget::new(8192)));
        let stores: Vec<_> = (0..MAX_OWNERS)
            .map(|index| {
                owners
                    .get(ResourceOwner::verified("issuer", &index.to_string()).unwrap())
                    .unwrap()
            })
            .collect();
        let extra = ResourceOwner::verified("issuer", "extra").unwrap();
        assert!(owners.get(extra.clone()).is_none());
        assert!(Arc::ptr_eq(
            &stores[0],
            &owners
                .get(ResourceOwner::verified("issuer", "0").unwrap())
                .unwrap()
        ));
        drop(stores);
        assert!(owners.get(extra).is_some());
    }
}
