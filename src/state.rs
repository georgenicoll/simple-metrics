//! What the sampler thread and the connection threads share.

use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

use crate::metrics::Schema;
use crate::store::Store;

/// The store and the facts about it that requests need. Shared between the
/// thread that adds records and the threads that answer requests.
#[derive(Debug)]
pub struct Shared {
    /// What each value in a record is.
    pub schema: Schema,
    /// The time between samples.
    pub interval: Duration,
    store: RwLock<Store>,
}

impl Shared {
    /// Shares `store`, whose records follow `schema`, sampled every `interval`.
    #[must_use]
    pub fn new(schema: Schema, interval: Duration, store: Store) -> Self {
        Self {
            schema,
            interval,
            store: RwLock::new(store),
        }
    }

    /// Read access to the store. Hold it only briefly: copy what's needed out
    /// and let go, so a slow client can never hold up the sampler.
    pub fn read(&self) -> RwLockReadGuard<'_, Store> {
        // The store can't be left half-updated by a panic (`push` checks
        // everything before changing anything), so a poisoned lock is safe
        // to use.
        self.store
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Write access to the store, for adding a record.
    pub fn write(&self) -> RwLockWriteGuard<'_, Store> {
        self.store
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
