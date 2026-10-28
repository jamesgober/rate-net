//! A keyed table of your own on the leaky-bucket arithmetic: one `GcraParams`
//! for the whole tier, one 8-byte word per key, stored beside the key.
//!
//! ```text
//! cargo run --example shared_params
//! ```

use core::sync::atomic::AtomicU64;
use core::time::Duration;
use rate_net::{GcraParams, Quota};

/// A tiny fixed table: each slot holds a key and its theoretical arrival
/// time, 16 bytes in all, so a lookup and a check touch one cache line.
struct Table {
    params: GcraParams,
    slots: Vec<(u64, AtomicU64)>,
}

impl Table {
    fn new(quota: Quota, keys: &[u64]) -> Self {
        Self {
            params: GcraParams::new(quota),
            // A word of 0 is a full bucket.
            slots: keys.iter().map(|&k| (k, AtomicU64::new(0))).collect(),
        }
    }

    fn check(&self, key: u64, now: Duration) -> Option<rate_net::Decision> {
        let (_, tat) = self.slots.iter().find(|(k, _)| *k == key)?;
        Some(self.params.check_at(tat, now))
    }
}

fn main() {
    // 5 per second per peer, bursts of 2.
    let table = Table::new(Quota::per_second(5).with_burst(2), &[10, 20, 30]);

    for ms in [0u64, 0, 0, 100, 200, 400] {
        let now = Duration::from_millis(ms);
        for key in [10, 20] {
            match table.check(key, now) {
                Some(decision) if decision.is_allow() => {
                    println!("{ms:>4} ms  peer {key}: allowed");
                }
                Some(decision) => println!(
                    "{ms:>4} ms  peer {key}: denied, retry in {:?}",
                    decision.retry_after().unwrap_or_default()
                ),
                None => println!("{ms:>4} ms  peer {key}: unknown"),
            }
        }
    }
    println!(
        "{} bytes of state per key, {} bytes of parameters for the tier",
        core::mem::size_of::<(u64, AtomicU64)>(),
        core::mem::size_of::<GcraParams>()
    );
}
