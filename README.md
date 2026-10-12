# limit-order-book

A limit order book and matching engine in Rust, built for speed: price-time
priority, array-indexed price levels, pooled order nodes, and no allocations
on the hot path. The design borrows from the OpenBook v2 slab layout, adapted
into a standalone zero-alloc core.

Day 1 of a 5-day build. The book structure is in place; matching lands next.

## Layout

- `src/types.rs` — `Side`, `Order`, and the tick/quantity/id aliases.
- `src/pool.rs` — intrusive node slab with a free list. `alloc` pops a slot,
  `free` pushes it back. The slab only grows when the free list runs dry.
- `src/book.rs` — the book itself: one dense level array per side, indexed
  directly by price tick.

## How it works

Prices are integer ticks in `[0, 2^20)`. Each side keeps a boxed array of
price levels, so finding a level is one index operation. Orders are stored in
the node pool and linked into their level's queue at the tail, which gives
time priority for free.

- `insert` rests an order: pop a node, link it at the level tail, update
  totals and the cached best bid/ask.
- `cancel` unlinks in O(1). It only scans for a new best price when the
  emptied level was the best one, which keeps the common case flat.
- Order ids map to node slots through a hash map for now. A dense id table
  is planned once the matching engine is in.

## Build and test

```sh
cargo build
cargo test
```

## Next

- Matching engine: cross the spread, walk levels, fill or rest.
- Self-trade behavior and order types (limit, IOC, FOK).
- Criterion benchmarks with flamegraphs, reporting p50/p99/p99.9 per order.
