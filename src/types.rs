//! Core domain types for the book.
//!
//! Prices are plain integer ticks so the hot path can index price levels
//! with a normal array instead of hashing or tree lookups.

/// Which side of the book an order rests on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Bid,
    Ask,
}

/// Price in integer ticks. The book treats this as an index, so callers are
/// expected to convert from decimal prices once, up front.
pub type Price = u64;

/// Quantity in base units.
pub type Qty = u64;

/// Client-assigned order id. Unique per order for the life of the book.
pub type OrderId = u64;

/// Identifies the trader (or session) behind an order. Only used for
/// self-trade prevention; the book never matches on it otherwise.
pub type TraderId = u32;

/// How a submitted order behaves once it hits the book.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderType {
    /// Fill whatever crosses, rest the remainder at the limit price.
    Limit,
    /// Fill whatever crosses, discard the remainder. Never rests.
    Ioc,
    /// Fill everything or nothing. Never rests, never partial.
    Fok,
}

/// An order as the book sees it. `seq` is assigned at insert time and decides
/// time priority inside a price level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Order {
    pub id: OrderId,
    pub side: Side,
    pub price: Price,
    pub qty: Qty,
    pub seq: u64,
    pub trader: TraderId,
}

/// One executed trade between an aggressive order and a resting order.
///
/// Fills are written into a caller-provided buffer so the matching path
/// never allocates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fill {
    /// The aggressive order that crossed the spread.
    pub taker_id: OrderId,
    /// The resting order that got hit.
    pub maker_id: OrderId,
    /// Execution price. Always the resting order's price, never the taker's.
    pub price: Price,
    pub qty: Qty,
}
