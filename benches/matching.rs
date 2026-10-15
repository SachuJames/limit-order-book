use criterion::{Criterion, black_box, criterion_group, criterion_main};
use limit_order_book::{Fill, OrderBook, OrderType, Side};
use std::time::{Duration, Instant};

fn fill_buf() -> [Fill; 64] {
    [Fill {
        taker_id: 0,
        maker_id: 0,
        price: 0,
        qty: 0,
    }; 64]
}

/// A book with some depth on both sides, ready to take crosses.
fn seeded() -> OrderBook {
    let mut book = OrderBook::new();
    for i in 0..2000u64 {
        book.insert(i, Side::Bid, 90_000 + (i % 500), 10, 1)
            .unwrap();
        book.insert(10_000 + i, Side::Ask, 100_000 + (i % 500), 10, 2)
            .unwrap();
    }
    book
}

/// Timed closure with `iter_batched` here included book setup in the
/// measurement, which drowned the per-order numbers (OrderBook::new zeroes
/// 48 MiB of level arrays). `iter_custom` keeps setup out of the clock.
fn bench_insert(c: &mut Criterion) {
    c.bench_function("insert resting order", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let mut book = OrderBook::new();
                let start = Instant::now();
                for i in 0..10u64 {
                    black_box(book.insert(i, Side::Bid, 100_000, 10, 1)).unwrap();
                }
                total += start.elapsed();
            }
            total
        });
    });
}

fn bench_submit_sweep(c: &mut Criterion) {
    c.bench_function("submit sweeping 5 levels", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let mut book = seeded();
                let mut fills = fill_buf();
                let start = Instant::now();
                // IOC so the taker never rests and the seeded book barely
                // changes shape between iterations.
                black_box(book.submit(
                    99_999,
                    Side::Bid,
                    200_000,
                    50,
                    3,
                    OrderType::Ioc,
                    &mut fills,
                ))
                .unwrap();
                total += start.elapsed();
            }
            total
        });
    });
}

fn bench_cancel(c: &mut Criterion) {
    c.bench_function("cancel resting order", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let mut book = seeded();
                let start = Instant::now();
                for i in 0..10u64 {
                    black_box(book.cancel(i)).unwrap();
                }
                total += start.elapsed();
            }
            total
        });
    });
}

criterion_group!(benches, bench_insert, bench_submit_sweep, bench_cancel);
criterion_main!(benches);
