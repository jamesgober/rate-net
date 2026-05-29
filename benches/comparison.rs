//! Head-to-head: `rate-net` vs [`governor`](https://crates.io/crates/governor).
//!
//! `governor` is a heavy, benchmark-only dependency, so it is gated on
//! `cfg(comparison)` rather than a Cargo feature — that keeps it out of the
//! default tree, `--all-features`, and CI. Run with:
//!
//! ```text
//! RUSTFLAGS="--cfg comparison" cargo bench --bench comparison
//! ```
//!
//! Without the cfg the bench compiles to a stub that prints how to enable it,
//! so a plain `cargo bench` still succeeds.

#[cfg(comparison)]
mod compare {
    use std::hint::black_box;
    use std::num::NonZeroU32;

    use criterion::Criterion;

    fn huge() -> NonZeroU32 {
        NonZeroU32::new(u32::MAX).expect("u32::MAX is non-zero")
    }

    pub fn single_key(c: &mut Criterion) {
        let rn = rate_net::RateLimiter::per_second(u32::MAX);
        let _ = rn.check(42u64);
        let gov = governor::RateLimiter::keyed(governor::Quota::per_second(huge()));
        let _ = gov.check_key(&42u64);

        let mut group = c.benchmark_group("compare/single_key");
        let _ = group.bench_function("rate-net", |b| b.iter(|| rn.check(black_box(42u64))));
        let _ = group.bench_function("governor", |b| b.iter(|| gov.check_key(black_box(&42u64))));
        group.finish();
    }

    pub fn many_keys(c: &mut Criterion) {
        let keys: Vec<u64> = (0..10_000).collect();

        let rn = rate_net::RateLimiter::per_second(u32::MAX);
        for &k in &keys {
            let _ = rn.check(k);
        }
        let gov = governor::RateLimiter::keyed(governor::Quota::per_second(huge()));
        for &k in &keys {
            let _ = gov.check_key(&k);
        }

        let mut group = c.benchmark_group("compare/many_keys");
        let _ = group.bench_function("rate-net", |b| {
            let mut i = 0usize;
            b.iter(|| {
                let k = keys[i % keys.len()];
                i = i.wrapping_add(1);
                rn.check(black_box(k))
            });
        });
        let _ = group.bench_function("governor", |b| {
            let mut i = 0usize;
            b.iter(|| {
                let k = keys[i % keys.len()];
                i = i.wrapping_add(1);
                gov.check_key(black_box(&k))
            });
        });
        group.finish();
    }
}

fn main() {
    #[cfg(comparison)]
    {
        let mut criterion = criterion::Criterion::default().configure_from_args();
        compare::single_key(&mut criterion);
        compare::many_keys(&mut criterion);
        criterion.final_summary();
    }
    #[cfg(not(comparison))]
    {
        eprintln!(
            "the governor comparison is gated; rebuild with \
             RUSTFLAGS=\"--cfg comparison\" cargo bench --bench comparison"
        );
    }
}
