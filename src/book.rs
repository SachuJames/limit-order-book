//! The order book.
//!
//! One dense array of price levels per side, indexed directly by tick.
//! Orders sit in the node pool and link into their level's intrusive queue,
//! appended at the tail so queue order is time priority. Insert and cancel
//! never allocate once the pool is warm.

use crate::id_table::IdTable;
use crate::pool::{NULL, NodePool};
use crate::types::{Fill, Order, OrderId, OrderType, Price, Qty, Side, TraderId};

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
    OrderIdOutOfRange,
}

pub struct OrderBook {
    bids: Box<[Level]>,
    asks: Box<[Level]>,
    pool: NodePool,
    ids: IdTable,
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
            ids: IdTable::new(),
            seq: 0,
            best_bid: None,
            best_ask: None,
        }
    }

    /// Rest an order on the book. No matching; use `submit` for that.
    pub fn insert(
        &mut self,
        id: OrderId,
        side: Side,
        price: Price,
        qty: Qty,
        trader: TraderId,
    ) -> Result<(), BookError> {
        if price >= MAX_TICKS as u64 {
            return Err(BookError::PriceOutOfRange);
        }
        if qty == 0 {
            return Err(BookError::ZeroQuantity);
        }
        if !IdTable::in_range(id) {
            return Err(BookError::OrderIdOutOfRange);
        }
        if self.ids.contains(id) {
            return Err(BookError::DuplicateOrder);
        }

        self.seq += 1;
        self.insert_order(Order {
            id,
            side,
            price,
            qty,
            seq: self.seq,
            trader,
        });
        Ok(())
    }

    /// Link a validated order into its level. Shared by `insert` and the
    /// resting remainder in `submit`.
    fn insert_order(&mut self, order: Order) {
        let node_idx = self.pool.alloc(&order);

        let levels = match order.side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };
        let level = &mut levels[order.price as usize];
        if level.tail == NULL {
            level.head = node_idx;
        } else {
            self.pool.get_mut(level.tail).next = node_idx;
        }
        self.pool.get_mut(node_idx).prev = level.tail;
        level.tail = node_idx;
        level.total_qty += order.qty;
        level.order_count += 1;

        self.ids.insert(order.id, node_idx);

        match order.side {
            Side::Bid => {
                self.best_bid = Some(self.best_bid.map_or(order.price, |b| b.max(order.price)))
            }
            Side::Ask => {
                self.best_ask = Some(self.best_ask.map_or(order.price, |a| a.min(order.price)))
            }
        }
    }

    /// Submit an order that may cross the spread.
    ///
    /// A buy crosses when its price reaches the best ask; a sell crosses
    /// when its price reaches the best bid. Levels are walked best-first and
    /// resting orders are consumed head-first, which is FIFO time priority.
    /// Every trade executes at the resting order's price.
    ///
    /// Fills are written into `fills` in execution order and the fill count
    /// is returned. Nothing is allocated on this path. If the buffer runs
    /// out, matching stops early.
    ///
    /// `kind` decides what happens to an unfilled remainder: `Limit` rests
    /// it at the limit price, `Ioc` drops it, and `Fok` never matches at
    /// all unless the whole quantity is available up front.
    ///
    /// Self-trade rule: if the aggressive order would cross a resting order
    /// from the same trader, the resting order is cancelled instead and no
    /// fill is printed. A trader can never buy from or sell to themselves.
    pub fn submit(
        &mut self,
        id: OrderId,
        side: Side,
        price: Price,
        qty: Qty,
        trader: TraderId,
        kind: OrderType,
        fills: &mut [Fill],
    ) -> Result<usize, BookError> {
        if price >= MAX_TICKS as u64 {
            return Err(BookError::PriceOutOfRange);
        }
        if qty == 0 {
            return Err(BookError::ZeroQuantity);
        }
        if !IdTable::in_range(id) {
            return Err(BookError::OrderIdOutOfRange);
        }
        if self.ids.contains(id) {
            return Err(BookError::DuplicateOrder);
        }

        if kind == OrderType::Fok && self.fillable_qty(side, price, trader) < qty {
            return Ok(0);
        }

        self.seq += 1;
        let taker_seq = self.seq;

        let mut remaining = qty;
        let mut n_fills = 0;

        while remaining > 0 && n_fills < fills.len() {
            let touch = match side {
                Side::Bid => self.best_ask,
                Side::Ask => self.best_bid,
            };
            let touch_price = match touch {
                Some(p) => match side {
                    Side::Bid => price >= p,
                    Side::Ask => price <= p,
                }
                .then_some(p),
                None => None,
            };
            let Some(touch_price) = touch_price else {
                break;
            };

            // Consume the level head-first: queue order is time priority.
            {
                let (levels, pool) = match side {
                    Side::Bid => (&mut self.asks, &mut self.pool),
                    Side::Ask => (&mut self.bids, &mut self.pool),
                };
                let level = &mut levels[touch_price as usize];
                while remaining > 0 && n_fills < fills.len() {
                    let head = level.head;
                    if head == NULL {
                        break;
                    }
                    let maker = *pool.get(head);
                    if maker.trader == trader {
                        // Self-trade: cancel the resting order, print no
                        // fill, and keep walking the level.
                        let next = maker.next;
                        level.head = next;
                        if next == NULL {
                            level.tail = NULL;
                        } else {
                            pool.get_mut(next).prev = NULL;
                        }
                        level.total_qty -= maker.qty;
                        level.order_count -= 1;
                        self.ids.remove(maker.id);
                        pool.free(head);
                        continue;
                    }
                    let fill_qty = remaining.min(maker.qty);
                    fills[n_fills] = Fill {
                        taker_id: id,
                        maker_id: maker.id,
                        price: touch_price,
                        qty: fill_qty,
                    };
                    n_fills += 1;
                    remaining -= fill_qty;
                    level.total_qty -= fill_qty;
                    if maker.qty == fill_qty {
                        let next = maker.next;
                        level.head = next;
                        if next == NULL {
                            level.tail = NULL;
                        } else {
                            pool.get_mut(next).prev = NULL;
                        }
                        level.order_count -= 1;
                        self.ids.remove(maker.id);
                        pool.free(head);
                    } else {
                        pool.get_mut(head).qty -= fill_qty;
                    }
                }
            }

            let emptied = match side {
                Side::Bid => self.asks[touch_price as usize].is_empty(),
                Side::Ask => self.bids[touch_price as usize].is_empty(),
            };
            if emptied {
                match side {
                    Side::Bid => self.best_ask = self.scan_up(touch_price),
                    Side::Ask => self.best_bid = self.scan_down(touch_price),
                }
            }
        }

        if remaining > 0 && kind == OrderType::Limit {
            self.insert_order(Order {
                id,
                side,
                price,
                qty: remaining,
                seq: taker_seq,
                trader,
            });
        }

        Ok(n_fills)
    }

    /// How much resting quantity could actually fill an aggressive order at
    /// this price: crossable levels, best first, skipping the trader's own
    /// orders (those get cancelled, not filled). Used for the FOK check.
    fn fillable_qty(&self, side: Side, price: Price, trader: TraderId) -> Qty {
        let mut total = 0u64;
        let mut touch = match side {
            Side::Bid => self.best_ask,
            Side::Ask => self.best_bid,
        };
        while let Some(touch_price) = touch {
            let crosses = match side {
                Side::Bid => price >= touch_price,
                Side::Ask => price <= touch_price,
            };
            if !crosses {
                break;
            }
            let levels = match side {
                Side::Bid => &self.asks,
                Side::Ask => &self.bids,
            };
            let mut cur = levels[touch_price as usize].head;
            while cur != NULL {
                let node = self.pool.get(cur);
                if node.trader != trader {
                    total = total.saturating_add(node.qty);
                }
                cur = node.next;
            }
            touch = match side {
                Side::Bid => self.scan_up(touch_price),
                Side::Ask => self.scan_down(touch_price),
            };
        }
        total
    }

    /// Pull an order off the book. O(1) unlink; only scans for a new best
    /// when the emptied level was the best one.
    pub fn cancel(&mut self, id: OrderId) -> Result<Order, BookError> {
        let node_idx = self.ids.get(id).ok_or(BookError::UnknownOrder)?;
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

        self.ids.remove(id);
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
            trader: node.trader,
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
                trader: n.trader,
            });
            cur = n.next;
        }
        out
    }

    /// Resting order count across both sides.
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
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
        book.insert(1, Side::Bid, 100, 10, 1).unwrap();
        book.insert(2, Side::Bid, 101, 5, 1).unwrap();
        book.insert(3, Side::Ask, 102, 7, 1).unwrap();
        book.insert(4, Side::Ask, 103, 3, 1).unwrap();
        assert_eq!(book.best_bid(), Some(101));
        assert_eq!(book.best_ask(), Some(102));
        assert_eq!(book.len(), 4);
    }

    #[test]
    fn time_priority_inside_a_level() {
        let mut book = OrderBook::new();
        book.insert(1, Side::Bid, 100, 10, 1).unwrap();
        book.insert(2, Side::Bid, 100, 20, 1).unwrap();
        book.insert(3, Side::Bid, 100, 30, 1).unwrap();
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
        book.insert(1, Side::Ask, 50, 10, 1).unwrap();
        book.insert(2, Side::Ask, 50, 20, 1).unwrap();
        book.insert(3, Side::Ask, 50, 30, 1).unwrap();
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
        book.insert(1, Side::Bid, 100, 10, 1).unwrap();
        book.insert(2, Side::Bid, 99, 10, 1).unwrap();
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
            book.insert(1, Side::Bid, MAX_TICKS as u64, 10, 1),
            Err(BookError::PriceOutOfRange)
        );
        assert_eq!(
            book.insert(1, Side::Bid, 100, 0, 1),
            Err(BookError::ZeroQuantity)
        );
        book.insert(1, Side::Bid, 100, 10, 1).unwrap();
        assert_eq!(
            book.insert(1, Side::Bid, 100, 10, 1),
            Err(BookError::DuplicateOrder)
        );
        assert_eq!(book.cancel(999), Err(BookError::UnknownOrder));
    }

    #[test]
    fn pool_slots_get_reused() {
        let mut book = OrderBook::new();
        for i in 0..100 {
            book.insert(i, Side::Bid, 100 + (i % 10), 1, 1).unwrap();
        }
        assert_eq!(book.pool_used(), 100);
        for i in 0..100 {
            book.cancel(i).unwrap();
        }
        assert_eq!(book.pool_used(), 0);
        for i in 100..200 {
            book.insert(i, Side::Bid, 100 + (i % 10), 1, 1).unwrap();
        }
        // No growth: the freed slots were reused.
        assert_eq!(book.pool_used(), 100);
        assert!(book.pool.capacity() <= 4096);
    }

    fn fill_buf() -> [Fill; 16] {
        [Fill {
            taker_id: 0,
            maker_id: 0,
            price: 0,
            qty: 0,
        }; 16]
    }

    #[test]
    fn cross_full_fill_clears_book() {
        let mut book = OrderBook::new();
        book.insert(1, Side::Ask, 100, 10, 1).unwrap();
        let mut fills = fill_buf();
        let n = book
            .submit(2, Side::Bid, 100, 10, 2, OrderType::Limit, &mut fills)
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            fills[0],
            Fill {
                taker_id: 2,
                maker_id: 1,
                price: 100,
                qty: 10,
            }
        );
        assert!(book.is_empty());
        assert_eq!(book.best_ask(), None);
        assert_eq!(book.best_bid(), None);
    }

    #[test]
    fn partial_fill_rests_remainder() {
        let mut book = OrderBook::new();
        book.insert(1, Side::Ask, 100, 10, 1).unwrap();
        let mut fills = fill_buf();
        let n = book
            .submit(2, Side::Bid, 100, 25, 2, OrderType::Limit, &mut fills)
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(fills[0].qty, 10);
        assert_eq!(book.len(), 1);
        assert_eq!(book.best_bid(), Some(100));
        assert_eq!(book.level_qty(Side::Bid, 100), 15);
        // The fully filled maker gave its pool slot back.
        assert_eq!(book.pool_used(), 1);
    }

    #[test]
    fn sweep_walks_multiple_levels_best_first() {
        let mut book = OrderBook::new();
        book.insert(1, Side::Ask, 100, 10, 1).unwrap();
        book.insert(2, Side::Ask, 101, 10, 1).unwrap();
        book.insert(3, Side::Ask, 102, 10, 1).unwrap();
        let mut fills = fill_buf();
        let n = book
            .submit(4, Side::Bid, 102, 25, 2, OrderType::Limit, &mut fills)
            .unwrap();
        assert_eq!(n, 3);
        assert_eq!((fills[0].price, fills[0].qty), (100, 10));
        assert_eq!((fills[1].price, fills[1].qty), (101, 10));
        assert_eq!((fills[2].price, fills[2].qty), (102, 5));
        assert_eq!(book.level_qty(Side::Ask, 102), 5);
        assert_eq!(book.best_ask(), Some(102));
        assert_eq!(book.pool_used(), 1);
    }

    #[test]
    fn better_price_matched_first() {
        let mut book = OrderBook::new();
        book.insert(1, Side::Ask, 102, 10, 1).unwrap();
        book.insert(2, Side::Ask, 100, 10, 1).unwrap();
        let mut fills = fill_buf();
        let n = book
            .submit(3, Side::Bid, 105, 10, 2, OrderType::Limit, &mut fills)
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(fills[0].maker_id, 2);
        assert_eq!(fills[0].price, 100);
    }

    #[test]
    fn fifo_within_a_level() {
        let mut book = OrderBook::new();
        book.insert(1, Side::Ask, 100, 5, 1).unwrap();
        book.insert(2, Side::Ask, 100, 5, 1).unwrap();
        book.insert(3, Side::Ask, 100, 5, 1).unwrap();
        let mut fills = fill_buf();
        let n = book
            .submit(4, Side::Bid, 100, 7, 2, OrderType::Limit, &mut fills)
            .unwrap();
        assert_eq!(n, 2);
        assert_eq!(fills[0].maker_id, 1);
        assert_eq!(fills[0].qty, 5);
        assert_eq!(fills[1].maker_id, 2);
        assert_eq!(fills[1].qty, 2);
        // Order 2 keeps 3 and stays at the head of the queue.
        let ids: Vec<OrderId> = book
            .level_orders(Side::Ask, 100)
            .iter()
            .map(|o| o.id)
            .collect();
        assert_eq!(ids, vec![2, 3]);
        assert_eq!(book.level_qty(Side::Ask, 100), 8);
    }

    #[test]
    fn executes_at_resting_price_not_taker_price() {
        let mut book = OrderBook::new();
        book.insert(1, Side::Ask, 100, 5, 1).unwrap();
        let mut fills = fill_buf();
        let n = book
            .submit(2, Side::Bid, 105, 5, 2, OrderType::Limit, &mut fills)
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(fills[0].price, 100);
        assert!(book.is_empty());
    }

    #[test]
    fn non_crossing_order_just_rests() {
        let mut book = OrderBook::new();
        book.insert(1, Side::Ask, 100, 10, 1).unwrap();
        let mut fills = fill_buf();
        let n = book
            .submit(2, Side::Bid, 99, 5, 2, OrderType::Limit, &mut fills)
            .unwrap();
        assert_eq!(n, 0);
        assert_eq!(book.best_bid(), Some(99));
        assert_eq!(book.best_ask(), Some(100));
        assert_eq!(book.len(), 2);
    }

    #[test]
    fn sell_taker_sweeps_bids() {
        let mut book = OrderBook::new();
        book.insert(1, Side::Bid, 100, 10, 1).unwrap();
        book.insert(2, Side::Bid, 99, 10, 1).unwrap();
        let mut fills = fill_buf();
        let n = book
            .submit(3, Side::Ask, 99, 15, 2, OrderType::Limit, &mut fills)
            .unwrap();
        assert_eq!(n, 2);
        assert_eq!((fills[0].price, fills[0].qty), (100, 10));
        assert_eq!((fills[1].price, fills[1].qty), (99, 5));
        assert_eq!(fills[0].taker_id, 3);
        assert_eq!(book.best_bid(), Some(99));
        assert_eq!(book.level_qty(Side::Bid, 99), 5);
    }

    #[test]
    fn submit_rejects_bad_input() {
        let mut book = OrderBook::new();
        let mut fills = fill_buf();
        assert_eq!(
            book.submit(
                1,
                Side::Bid,
                MAX_TICKS as u64,
                10,
                0,
                OrderType::Limit,
                &mut fills
            ),
            Err(BookError::PriceOutOfRange)
        );
        assert_eq!(
            book.submit(1, Side::Bid, 100, 0, 2, OrderType::Limit, &mut fills),
            Err(BookError::ZeroQuantity)
        );
        book.insert(1, Side::Ask, 100, 10, 1).unwrap();
        assert_eq!(
            book.submit(1, Side::Bid, 100, 10, 2, OrderType::Limit, &mut fills),
            Err(BookError::DuplicateOrder)
        );
    }

    #[test]
    fn filled_maker_cannot_be_cancelled() {
        let mut book = OrderBook::new();
        book.insert(1, Side::Ask, 100, 10, 1).unwrap();
        let mut fills = fill_buf();
        let n = book
            .submit(2, Side::Bid, 100, 10, 2, OrderType::Limit, &mut fills)
            .unwrap();
        assert_eq!(n, 1);
        // The fill removed the maker from the id table as well as the level.
        assert_eq!(book.cancel(1), Err(BookError::UnknownOrder));
        assert!(book.is_empty());
    }

    #[test]
    fn cancel_after_partial_fill_returns_remaining_qty() {
        let mut book = OrderBook::new();
        book.insert(1, Side::Ask, 100, 10, 1).unwrap();
        let mut fills = fill_buf();
        let n = book
            .submit(2, Side::Bid, 100, 4, 2, OrderType::Limit, &mut fills)
            .unwrap();
        assert_eq!(n, 1);
        let gone = book.cancel(1).unwrap();
        assert_eq!(gone.qty, 6);
        assert!(book.is_empty());
    }

    #[test]
    fn order_id_reusable_once_off_the_book() {
        let mut book = OrderBook::new();
        book.insert(1, Side::Ask, 100, 5, 1).unwrap();
        let mut fills = fill_buf();
        book.submit(2, Side::Bid, 100, 5, 2, OrderType::Limit, &mut fills)
            .unwrap();
        // Id 1 fully filled: free to use again.
        book.insert(1, Side::Ask, 101, 5, 1).unwrap();
        book.cancel(1).unwrap();
        // And free again after a cancel.
        book.insert(1, Side::Bid, 99, 5, 1).unwrap();
        assert_eq!(book.len(), 1);
        assert_eq!(book.best_bid(), Some(99));
    }

    #[test]
    fn full_fill_buffer_stops_matching_and_rests_remainder() {
        let mut book = OrderBook::new();
        book.insert(1, Side::Ask, 100, 5, 1).unwrap();
        book.insert(2, Side::Ask, 100, 5, 1).unwrap();
        let mut fills = [Fill {
            taker_id: 0,
            maker_id: 0,
            price: 0,
            qty: 0,
        }; 1];
        let n = book
            .submit(3, Side::Bid, 100, 10, 2, OrderType::Limit, &mut fills)
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            fills[0],
            Fill {
                taker_id: 3,
                maker_id: 1,
                price: 100,
                qty: 5,
            }
        );
        // Matching stopped at the buffer limit: maker 2 is untouched and
        // the taker's remaining 5 rest as a bid.
        assert_eq!(book.level_qty(Side::Ask, 100), 5);
        assert_eq!(book.level_qty(Side::Bid, 100), 5);
        assert_eq!(book.len(), 2);
    }

    #[test]
    fn empty_fill_buffer_rests_without_matching() {
        let mut book = OrderBook::new();
        book.insert(1, Side::Ask, 100, 10, 1).unwrap();
        let n = book
            .submit(2, Side::Bid, 100, 5, 0, OrderType::Limit, &mut [])
            .unwrap();
        assert_eq!(n, 0);
        assert_eq!(book.level_qty(Side::Ask, 100), 10);
        assert_eq!(book.level_qty(Side::Bid, 100), 5);
        assert_eq!(book.len(), 2);
    }

    #[test]
    fn large_order_id_roundtrip() {
        let mut book = OrderBook::new();
        book.insert(1_000_000, Side::Bid, 100, 10, 1).unwrap();
        assert_eq!(book.len(), 1);
        let gone = book.cancel(1_000_000).unwrap();
        assert_eq!(gone.id, 1_000_000);
        assert!(book.is_empty());
    }

    #[test]
    fn order_id_out_of_range_rejected() {
        let mut book = OrderBook::new();
        let big = crate::MAX_IDS as OrderId;
        assert_eq!(
            book.insert(big, Side::Bid, 100, 10, 1),
            Err(BookError::OrderIdOutOfRange)
        );
        let mut fills = fill_buf();
        assert_eq!(
            book.submit(big, Side::Bid, 100, 10, 2, OrderType::Limit, &mut fills),
            Err(BookError::OrderIdOutOfRange)
        );
        assert!(book.is_empty());
    }

    #[test]
    fn limit_resting_remainder_keeps_trader() {
        let mut book = OrderBook::new();
        book.insert(1, Side::Ask, 100, 10, 1).unwrap();
        let mut fills = fill_buf();
        let n = book
            .submit(2, Side::Bid, 100, 25, 2, OrderType::Limit, &mut fills)
            .unwrap();
        assert_eq!(n, 1);
        let resting: Vec<Order> = book.level_orders(Side::Bid, 100);
        assert_eq!(resting.len(), 1);
        assert_eq!(resting[0].id, 2);
        assert_eq!(resting[0].qty, 15);
        assert_eq!(resting[0].trader, 2);
    }

    #[test]
    fn ioc_fills_what_it_can_then_vanishes() {
        let mut book = OrderBook::new();
        book.insert(1, Side::Ask, 100, 10, 1).unwrap();
        let mut fills = fill_buf();
        let n = book
            .submit(2, Side::Bid, 100, 25, 2, OrderType::Ioc, &mut fills)
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(fills[0].qty, 10);
        assert!(book.is_empty());
        assert_eq!(book.best_bid(), None);
        assert_eq!(book.best_ask(), None);
    }

    #[test]
    fn ioc_with_nothing_crossing_does_nothing() {
        let mut book = OrderBook::new();
        book.insert(1, Side::Ask, 100, 10, 1).unwrap();
        let mut fills = fill_buf();
        let n = book
            .submit(2, Side::Bid, 99, 5, 2, OrderType::Ioc, &mut fills)
            .unwrap();
        assert_eq!(n, 0);
        // The ask is untouched and nothing rested.
        assert_eq!(book.len(), 1);
        assert_eq!(book.best_bid(), None);
    }

    #[test]
    fn fok_full_fill_leaves_no_remainder() {
        let mut book = OrderBook::new();
        book.insert(1, Side::Ask, 100, 10, 1).unwrap();
        book.insert(2, Side::Ask, 101, 10, 1).unwrap();
        let mut fills = fill_buf();
        let n = book
            .submit(3, Side::Bid, 101, 20, 2, OrderType::Fok, &mut fills)
            .unwrap();
        assert_eq!(n, 2);
        assert_eq!((fills[0].price, fills[0].qty), (100, 10));
        assert_eq!((fills[1].price, fills[1].qty), (101, 10));
        assert!(book.is_empty());
    }

    #[test]
    fn fok_shortfall_kills_the_whole_order() {
        let mut book = OrderBook::new();
        book.insert(1, Side::Ask, 100, 10, 1).unwrap();
        book.insert(2, Side::Ask, 101, 5, 1).unwrap();
        let mut fills = fill_buf();
        // Only 15 of the 20 wanted are on the book.
        let n = book
            .submit(3, Side::Bid, 101, 20, 2, OrderType::Fok, &mut fills)
            .unwrap();
        assert_eq!(n, 0);
        // Nothing traded, nothing rested, the book is exactly as it was.
        assert_eq!(book.len(), 2);
        assert_eq!(book.level_qty(Side::Ask, 100), 10);
        assert_eq!(book.level_qty(Side::Ask, 101), 5);
        assert_eq!(book.pool_used(), 2);
    }

    #[test]
    fn fok_never_rests_even_when_empty() {
        let mut book = OrderBook::new();
        let mut fills = fill_buf();
        let n = book
            .submit(1, Side::Bid, 100, 10, 2, OrderType::Fok, &mut fills)
            .unwrap();
        assert_eq!(n, 0);
        assert!(book.is_empty());
    }

    #[test]
    fn self_trade_cancels_resting_order_no_fill() {
        let mut book = OrderBook::new();
        book.insert(1, Side::Ask, 100, 10, 7).unwrap();
        let mut fills = fill_buf();
        // Same trader buys across their own ask: the ask dies, no fill prints.
        let n = book
            .submit(2, Side::Bid, 100, 10, 7, OrderType::Limit, &mut fills)
            .unwrap();
        assert_eq!(n, 0);
        assert_eq!(book.cancel(1), Err(BookError::UnknownOrder));
        // The remainder rested like a normal limit order.
        assert_eq!(book.level_qty(Side::Bid, 100), 10);
    }

    #[test]
    fn self_trade_spares_other_traders_orders() {
        let mut book = OrderBook::new();
        // Trader 1's ask is at the head of the level, trader 2's behind it.
        book.insert(1, Side::Ask, 100, 5, 1).unwrap();
        book.insert(2, Side::Ask, 100, 10, 2).unwrap();
        let mut fills = fill_buf();
        // Trader 1 buys across: their own ask is cancelled, trader 2's ask
        // still fills normally at the head of the queue afterwards.
        let n = book
            .submit(3, Side::Bid, 100, 10, 1, OrderType::Limit, &mut fills)
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            fills[0],
            Fill {
                taker_id: 3,
                maker_id: 2,
                price: 100,
                qty: 10,
            }
        );
        assert!(book.is_empty());
        assert_eq!(book.cancel(1), Err(BookError::UnknownOrder));
    }

    #[test]
    fn self_trade_keeps_walking_past_own_orders() {
        let mut book = OrderBook::new();
        book.insert(1, Side::Ask, 100, 5, 1).unwrap();
        book.insert(2, Side::Ask, 101, 10, 2).unwrap();
        let mut fills = fill_buf();
        // Own order at 100 is cancelled and the sweep continues to 101.
        let n = book
            .submit(3, Side::Bid, 101, 8, 1, OrderType::Limit, &mut fills)
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!((fills[0].maker_id, fills[0].qty), (2, 8));
        assert_eq!(book.level_qty(Side::Ask, 101), 2);
        assert_eq!(book.level_qty(Side::Ask, 100), 0);
        assert_eq!(book.best_ask(), Some(101));
    }

    #[test]
    fn self_trade_cancelled_id_can_be_reused() {
        let mut book = OrderBook::new();
        book.insert(1, Side::Ask, 100, 5, 1).unwrap();
        let mut fills = fill_buf();
        book.submit(2, Side::Bid, 100, 5, 1, OrderType::Limit, &mut fills)
            .unwrap();
        // Order 1 was cancelled as a self-trade: its id is free again.
        // Note the taker's own bid remainder already rests at 100.
        book.insert(1, Side::Bid, 99, 5, 1).unwrap();
        assert_eq!(book.len(), 2);
        assert_eq!(book.best_bid(), Some(100));
    }

    #[test]
    fn fok_counts_only_other_traders_qty() {
        let mut book = OrderBook::new();
        book.insert(1, Side::Ask, 100, 100, 1).unwrap();
        let mut fills = fill_buf();
        // The book shows 100, but it is all trader 1's own size, so a 20-lot
        // FOK from trader 1 finds nothing fillable and dies.
        let n = book
            .submit(2, Side::Bid, 100, 20, 1, OrderType::Fok, &mut fills)
            .unwrap();
        assert_eq!(n, 0);
        assert_eq!(book.level_qty(Side::Ask, 100), 100);
    }
}
