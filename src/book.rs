//! The order book.
//!
//! One dense array of price levels per side, indexed directly by tick.
//! Orders sit in the node pool and link into their level's intrusive queue,
//! appended at the tail so queue order is time priority. Insert and cancel
//! never allocate once the pool is warm.

use std::collections::HashMap;

use crate::pool::{NULL, NodePool};
use crate::types::{Order, OrderId, Price, Qty, Side};

/// Ticks live in `[0, MAX_TICKS)`. 2^20 levels at 24 bytes each is 24 MiB per
/// side, allocated once; the OS only faults pages in as levels get touched.
pub const MAX_TICKS: usize = 1 << 20;

#[derive(Clone, Copy, Debug)]
struct Level {
    head: u32,
    tail: u32,
    total_qty: Qty,
    order_count: u32,
}

impl Level {
    const fn empty() -> Self {
        Level {
            head: NULL,
            tail: NULL,
            total_qty: 0,
            order_count: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.head == NULL
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum BookError {
    PriceOutOfRange,
    ZeroQuantity,
    DuplicateOrder,
    UnknownOrder,
}

pub struct OrderBook {
    bids: Box<[Level]>,
    asks: Box<[Level]>,
    pool: NodePool,
    id_to_node: HashMap<OrderId, u32>,
    seq: u64,
    best_bid: Option<Price>,
    best_ask: Option<Price>,
}

impl OrderBook {
    pub fn new() -> Self {
        OrderBook {
            bids: vec![Level::empty(); MAX_TICKS].into_boxed_slice(),
            asks: vec![Level::empty(); MAX_TICKS].into_boxed_slice(),
            pool: NodePool::with_capacity(4096),
            id_to_node: HashMap::new(),
            seq: 0,
            best_bid: None,
            best_ask: None,
        }
    }

    /// Rest an order on the book. No matching yet; that lands tomorrow.
    pub fn insert(
        &mut self,
        id: OrderId,
        side: Side,
        price: Price,
        qty: Qty,
    ) -> Result<(), BookError> {
        if price >= MAX_TICKS as u64 {
            return Err(BookError::PriceOutOfRange);
        }
        if qty == 0 {
            return Err(BookError::ZeroQuantity);
        }
        if self.id_to_node.contains_key(&id) {
            return Err(BookError::DuplicateOrder);
        }

        self.seq += 1;
        let order = Order {
            id,
            side,
            price,
            qty,
            seq: self.seq,
        };
        let node_idx = self.pool.alloc(&order);

        let levels = match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };
        let level = &mut levels[price as usize];
        if level.tail == NULL {
            level.head = node_idx;
        } else {
            self.pool.get_mut(level.tail).next = node_idx;
        }
        self.pool.get_mut(node_idx).prev = level.tail;
        level.tail = node_idx;
        level.total_qty += qty;
        level.order_count += 1;

        self.id_to_node.insert(id, node_idx);

        match side {
            Side::Bid => self.best_bid = Some(self.best_bid.map_or(price, |b| b.max(price))),
            Side::Ask => self.best_ask = Some(self.best_ask.map_or(price, |a| a.min(price))),
        }
        Ok(())
    }

    /// Pull an order off the book. O(1) unlink; only scans for a new best
    /// when the emptied level was the best one.
    pub fn cancel(&mut self, id: OrderId) -> Result<Order, BookError> {
        let node_idx = *self.id_to_node.get(&id).ok_or(BookError::UnknownOrder)?;
        let node = *self.pool.get(node_idx);

        let (levels, pool) = match node.side {
            Side::Bid => (&mut self.bids, &mut self.pool),
            Side::Ask => (&mut self.asks, &mut self.pool),
        };
        let level = &mut levels[node.price as usize];

        if node.prev == NULL {
            level.head = node.next;
        } else {
            pool.get_mut(node.prev).next = node.next;
        }
        if node.next == NULL {
            level.tail = node.prev;
        } else {
            pool.get_mut(node.next).prev = node.prev;
        }
        level.total_qty -= node.qty;
        level.order_count -= 1;

        self.id_to_node.remove(&id);
        self.pool.free(node_idx);

        if level.is_empty() {
            match node.side {
                Side::Bid if self.best_bid == Some(node.price) => {
                    self.best_bid = self.scan_down(node.price)
                }
                Side::Ask if self.best_ask == Some(node.price) => {
                    self.best_ask = self.scan_up(node.price)
                }
                _ => {}
            }
        }

        Ok(Order {
            id: node.id,
            side: node.side,
            price: node.price,
            qty: node.qty,
            seq: node.seq,
        })
    }

    fn scan_down(&self, from: Price) -> Option<Price> {
        let mut p = from;
        while p > 0 {
            p -= 1;
            if !self.bids[p as usize].is_empty() {
                return Some(p);
            }
        }
        None
    }

    fn scan_up(&self, from: Price) -> Option<Price> {
        let mut p = from + 1;
        while p < MAX_TICKS as u64 {
            if !self.asks[p as usize].is_empty() {
                return Some(p);
            }
            p += 1;
        }
        None
    }

    pub fn best_bid(&self) -> Option<Price> {
        self.best_bid
    }

    pub fn best_ask(&self) -> Option<Price> {
        self.best_ask
    }

    /// Total resting quantity at a level.
    pub fn level_qty(&self, side: Side, price: Price) -> Qty {
        if price >= MAX_TICKS as u64 {
            return 0;
        }
        match side {
            Side::Bid => self.bids[price as usize].total_qty,
            Side::Ask => self.asks[price as usize].total_qty,
        }
    }

    /// Orders at a level in priority order. Allocates; for tests and
    /// debugging only, not the hot path.
    pub fn level_orders(&self, side: Side, price: Price) -> Vec<Order> {
        let mut out = Vec::new();
        if price >= MAX_TICKS as u64 {
            return out;
        }
        let levels = match side {
            Side::Bid => &self.bids,
            Side::Ask => &self.asks,
        };
        let mut cur = levels[price as usize].head;
        while cur != NULL {
            let n = self.pool.get(cur);
            out.push(Order {
                id: n.id,
                side: n.side,
                price: n.price,
                qty: n.qty,
                seq: n.seq,
            });
            cur = n.next;
        }
        out
    }

    /// Resting order count across both sides.
    pub fn len(&self) -> usize {
        self.id_to_node.len()
    }

    pub fn is_empty(&self) -> bool {
        self.id_to_node.is_empty()
    }

    /// Pool slots in use right now. Test hook.
    #[cfg(test)]
    fn pool_used(&self) -> usize {
        self.pool.capacity() - self.pool.free_count()
    }
}

impl Default for OrderBook {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn best_bid_ask_track_inserts() {
        let mut book = OrderBook::new();
        book.insert(1, Side::Bid, 100, 10).unwrap();
        book.insert(2, Side::Bid, 101, 5).unwrap();
        book.insert(3, Side::Ask, 102, 7).unwrap();
        book.insert(4, Side::Ask, 103, 3).unwrap();
        assert_eq!(book.best_bid(), Some(101));
        assert_eq!(book.best_ask(), Some(102));
        assert_eq!(book.len(), 4);
    }

    #[test]
    fn time_priority_inside_a_level() {
        let mut book = OrderBook::new();
        book.insert(1, Side::Bid, 100, 10).unwrap();
        book.insert(2, Side::Bid, 100, 20).unwrap();
        book.insert(3, Side::Bid, 100, 30).unwrap();
        let ids: Vec<OrderId> = book
            .level_orders(Side::Bid, 100)
            .iter()
            .map(|o| o.id)
            .collect();
        assert_eq!(ids, vec![1, 2, 3]);
        assert_eq!(book.level_qty(Side::Bid, 100), 60);
    }

    #[test]
    fn cancel_middle_keeps_queue_order() {
        let mut book = OrderBook::new();
        book.insert(1, Side::Ask, 50, 10).unwrap();
        book.insert(2, Side::Ask, 50, 20).unwrap();
        book.insert(3, Side::Ask, 50, 30).unwrap();
        let gone = book.cancel(2).unwrap();
        assert_eq!(gone.qty, 20);
        let ids: Vec<OrderId> = book
            .level_orders(Side::Ask, 50)
            .iter()
            .map(|o| o.id)
            .collect();
        assert_eq!(ids, vec![1, 3]);
        assert_eq!(book.level_qty(Side::Ask, 50), 40);
    }

    #[test]
    fn cancel_empty_best_moves_best() {
        let mut book = OrderBook::new();
        book.insert(1, Side::Bid, 100, 10).unwrap();
        book.insert(2, Side::Bid, 99, 10).unwrap();
        book.cancel(1).unwrap();
        assert_eq!(book.best_bid(), Some(99));
        book.cancel(2).unwrap();
        assert_eq!(book.best_bid(), None);
        assert!(book.is_empty());
    }

    #[test]
    fn rejects_bad_input() {
        let mut book = OrderBook::new();
        assert_eq!(
            book.insert(1, Side::Bid, MAX_TICKS as u64, 10),
            Err(BookError::PriceOutOfRange)
        );
        assert_eq!(
            book.insert(1, Side::Bid, 100, 0),
            Err(BookError::ZeroQuantity)
        );
        book.insert(1, Side::Bid, 100, 10).unwrap();
        assert_eq!(
            book.insert(1, Side::Bid, 100, 10),
            Err(BookError::DuplicateOrder)
        );
        assert_eq!(book.cancel(999), Err(BookError::UnknownOrder));
    }

    #[test]
    fn pool_slots_get_reused() {
        let mut book = OrderBook::new();
        for i in 0..100 {
            book.insert(i, Side::Bid, 100 + (i % 10), 1).unwrap();
        }
        assert_eq!(book.pool_used(), 100);
        for i in 0..100 {
            book.cancel(i).unwrap();
        }
        assert_eq!(book.pool_used(), 0);
        for i in 100..200 {
            book.insert(i, Side::Bid, 100 + (i % 10), 1).unwrap();
        }
        // No growth: the freed slots were reused.
        assert_eq!(book.pool_used(), 100);
        assert!(book.pool.capacity() <= 4096);
    }
}
