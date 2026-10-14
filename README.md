# limit-order-book

A limit order book and matching engine in Rust, built for speed: price-time
priority, array-indexed price levels, pooled order nodes, and no allocations
on the hot path. The design borrows from the OpenBook v2 slab layout, adapted
into a standalone zero-alloc core.

Day 2 of a 5-day build. The matching engine is in: aggressive orders cross the
spread, walk price levels best-first, and fill resting orders FIFO at the
resting price. Unfilled remainders rest at the limit price.

## Layout

- `src/types.rs` — `Side`, `Order`, `Fill`, and the tick/quantity/id aliases.
- `src/pool.rs` — intrusive node slab with a free list. `alloc` pops a slot,
  `free` pushes it back. The slab only grows when the free list runs dry.
- `src/book.rs` — the book itself: one dense level array per side, indexed
  directly by price tick.
- `src/id_table.rs` — dense order-id index: id to pool slot in one array
  lookup, grown geometrically as larger ids arrive.

## How it works

Prices are integer ticks in `[0, 2^20)`. Each side keeps a boxed array of
price levels, so finding a level is one index operation. Orders are stored in
the node pool and linked into their level's queue at the tail, which gives
time priority for free.

- `insert` rests an order: pop a node, link it at the level tail, update
  totals and the cached best bid/ask.
- `cancel` unlinks in O(1). It only scans for a new best price when the
  emptied level was the best one, which keeps the common case flat.
- `submit` takes an aggressive order plus a caller-provided fill buffer and
  returns the fill count. A buy crosses when its price reaches the best ask,
  a sell when its price reaches the best bid. Levels are consumed best-first
  and head-first (time priority), every trade prints at the resting price,
  and whatever is left over rests at the limit price. The buffer is the only
  output channel, so the hot path stays allocation-free. If the buffer runs
  out, matching stops and the remainder rests.
- Order ids map to node slots through the dense id table in
  `src/id_table.rs`: one array lookup, no hashing on the cancel or
  duplicate-check path. The table doubles as larger ids arrive, so memory
  tracks the largest live id rather than the order count. Ids are u64 on
  the wire but the index caps at 2^24; `insert` and `submit` reject ids
  past that with `OrderIdOutOfRange` instead of forcing a giant
  allocation.

## Build and test

```sh
cargo build
cargo test
```

## Next

- Self-trade behavior and order types (limit, IOC, FOK).
- Criterion benchmarks with flamegraphs, reporting p50/p99/p99.9 per order.
