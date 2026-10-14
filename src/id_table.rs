//! Dense order-id index.
//!
//! Maps an order id straight to its pool node with one array lookup, so
//! cancel and the duplicate check never hash. The table starts small and
//! doubles as larger ids arrive; memory stays proportional to the largest
//! live id, which is the usual dense-index trade.
//!
//! Order ids are u64 on the wire, but a dense index has to stop somewhere:
//! ids at or above `MAX_IDS` are refused rather than forcing a huge
//! allocation. 2^24 ids is 64 MiB of slots at full size and far beyond
//! what a single book session carries.

use crate::pool::NULL;
use crate::types::OrderId;

/// Largest id space the table will grow to cover.
pub const MAX_IDS: usize = 1 << 24;

const INITIAL_CAPACITY: usize = 1024;

pub struct IdTable {
    slots: Vec<u32>,
    len: usize,
}

impl IdTable {
    pub fn new() -> Self {
        IdTable {
            slots: vec![NULL; INITIAL_CAPACITY],
            len: 0,
        }
    }

    pub fn get(&self, id: OrderId) -> Option<u32> {
        let slot = *self.slots.get(id as usize)?;
        if slot == NULL { None } else { Some(slot) }
    }

    pub fn contains(&self, id: OrderId) -> bool {
        self.get(id).is_some()
    }

    /// Whether `id` falls inside the dense range. Callers check this at
    /// the API boundary; `insert` treats an out-of-range id as a bug.
    pub fn in_range(id: OrderId) -> bool {
        (id as usize) < MAX_IDS
    }

    /// Record `id -> node`. The id must be in range and not already live.
    pub fn insert(&mut self, id: OrderId, node: u32) {
        debug_assert!(Self::in_range(id));
        debug_assert!(!self.contains(id));
        let idx = id as usize;
        if idx >= self.slots.len() {
            let mut cap = self.slots.len();
            while cap <= idx {
                cap = (cap * 2).min(MAX_IDS);
            }
            self.slots.resize(cap, NULL);
        }
        self.slots[idx] = node;
        self.len += 1;
    }

    /// Drop `id`, returning its node if it was live.
    pub fn remove(&mut self, id: OrderId) -> Option<u32> {
        let idx = id as usize;
        let slot = *self.slots.get(idx)?;
        if slot == NULL {
            return None;
        }
        self.slots[idx] = NULL;
        self.len -= 1;
        Some(slot)
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Default for IdTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_get_remove_roundtrip() {
        let mut table = IdTable::new();
        assert_eq!(table.get(7), None);
        table.insert(7, 42);
        assert_eq!(table.get(7), Some(42));
        assert!(table.contains(7));
        assert_eq!(table.len(), 1);
        assert_eq!(table.remove(7), Some(42));
        assert_eq!(table.get(7), None);
        assert!(table.is_empty());
        // Removing twice is a no-op, not a second hit.
        assert_eq!(table.remove(7), None);
    }

    #[test]
    fn grows_past_initial_capacity() {
        let mut table = IdTable::new();
        table.insert(5000, 11);
        table.insert(3, 22);
        assert_eq!(table.get(5000), Some(11));
        assert_eq!(table.get(3), Some(22));
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn top_of_range_is_insertable() {
        let mut table = IdTable::new();
        let top = MAX_IDS as OrderId - 1;
        assert!(IdTable::in_range(top));
        table.insert(top, 99);
        assert_eq!(table.get(top), Some(99));
        assert_eq!(table.remove(top), Some(99));
    }

    #[test]
    fn range_boundary() {
        assert!(IdTable::in_range(0));
        assert!(IdTable::in_range(MAX_IDS as OrderId - 1));
        assert!(!IdTable::in_range(MAX_IDS as OrderId));
        assert!(!IdTable::in_range(OrderId::MAX));
        // Out-of-range lookups miss cleanly instead of panicking.
        let table = IdTable::new();
        assert_eq!(table.get(OrderId::MAX), None);
    }

    #[test]
    fn slot_reuse_after_remove() {
        let mut table = IdTable::new();
        table.insert(9, 1);
        table.remove(9);
        table.insert(9, 2);
        assert_eq!(table.get(9), Some(2));
        assert_eq!(table.len(), 1);
    }
}
