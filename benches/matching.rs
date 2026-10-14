use criterion::{BatchSize, Criterion, black_box, criterion_group, criterion_main};
use limit_order_book::{Fill, OrderBook, OrderType, Side};

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

fn bench_insert(c: &mut Criterion) {
    c.bench_function("insert resting order", |b| {
        b.iter_batched(
            OrderBook::new,
            |mut book| {
                for i in 0..10u64 {
                    black_box(book.insert(i, Side::Bid, 100_000, 10, 1)).unwrap();
                }
            },
            BatchSize::SmallInput,
        );
    });
}

fn bench_submit_sweep(c: &mut Criterion) {
    c.bench_function("submit sweeping 5 levels", |b| {
        b.iter_batched(
            seeded,
            |mut book| {
                let mut fills = fill_buf();
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
                .unwrap()
            },
            BatchSize::SmallInput,
        );
    });
}

fn bench_cancel(c: &mut Criterion) {
    c.bench_function("cancel resting order", |b| {
        b.iter_batched(
            seeded,
            |mut book| {
                for i in 0..10u64 {
                    black_box(book.cancel(i)).unwrap();
                }
            },
            BatchSize::SmallInput,
        );
    });
}

criterion_group!(benches, bench_insert, bench_submit_sweep, bench_cancel);
criterion_main!(benches);
