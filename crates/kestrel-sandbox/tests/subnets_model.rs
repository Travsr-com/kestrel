//! Issue #34: `SubnetPool` against a simple model. Random, seeded sequences of reserve, release,
//! double release, `exclude` and `exclude_except` on a /26 pool (8 blocks), with foreign subnets
//! from /16 to /32 (some partly outside the pool) and keep sets that include stale handles. After
//! every step the pool must agree with the model:
//!
//! * `reserve` hands out the lowest block that is neither held nor covered by a foreign subnet, so
//!   no block is handed out twice and none is lost unless a foreign subnet covers it;
//! * `available` is the free, uncovered blocks; `in_use` is the held, uncovered blocks;
//! * the count `exclude` and `exclude_except` return is never below the blocks newly covered.
//!
//! Deterministic: the same seeds run every time, and a failure prints the operations that led to it.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;

use kestrel_sandbox::{Ipv4Cidr, Subnet, SubnetPool};

/// A small linear congruential generator, so the test needs no extra dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const POOL: &str = "10.200.0.0/26";
const BASE: u32 = u32::from_be_bytes([10, 200, 0, 0]);
/// Blocks in the pool: a /26 holds eight /29 blocks.
const BLOCKS: u32 = 8;
const SEEDS: u64 = 2_000;
const STEPS: usize = 40;

/// `addr/prefix` with the host bits cleared.
fn cidr(addr: u32, prefix: u8) -> Ipv4Cidr {
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    Ipv4Cidr::parse(&format!("{}/{prefix}", Ipv4Addr::from(addr & mask))).unwrap()
}

/// The pool's block indexes that `c` overlaps.
fn blocks_of(c: &Ipv4Cidr) -> Vec<u32> {
    let start = u64::from(u32::from(c.addr()));
    let end = start + (1u64 << (32 - c.prefix())) - 1;
    (0..BLOCKS)
        .filter(|b| {
            let first = u64::from(BASE) + u64::from(*b) * 8;
            first <= end && start <= first + 7
        })
        .collect()
}

fn index(s: &Subnet) -> u32 {
    (u32::from(s.cidr().addr()) - BASE) / 8
}

/// A foreign subnet: mostly small ones inside the pool, some straddling its start, some covering
/// all of it.
fn random_taken(r: &mut Rng) -> Ipv4Cidr {
    match r.below(10) {
        0 => cidr(BASE - 8 + r.below(16) as u32 * 4, 28 + r.below(3) as u8),
        1 => cidr(BASE, 16 + r.below(10) as u8),
        _ => cidr(BASE + r.below(64) as u32, 26 + r.below(7) as u8),
    }
}

/// The model: blocks held (by index, with the handle) and blocks a foreign subnet covers.
#[derive(Default)]
struct Model {
    held: BTreeMap<u32, Subnet>,
    released: Vec<Subnet>,
    foreign: BTreeSet<u32>,
}

impl Model {
    fn lowest_free(&self) -> Option<u32> {
        (0..BLOCKS).find(|b| !self.held.contains_key(b) && !self.foreign.contains(b))
    }

    fn available(&self) -> usize {
        (0..BLOCKS)
            .filter(|b| !self.held.contains_key(b) && !self.foreign.contains(b))
            .count()
    }

    fn in_use(&self) -> usize {
        self.held
            .keys()
            .filter(|b| !self.foreign.contains(b))
            .count()
    }
}

fn run_seed(seed: u64) {
    let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1));
    let pool = SubnetPool::parse(POOL).unwrap();
    let mut m = Model::default();
    let mut log = String::new();
    for _ in 0..STEPS {
        match r.below(6) {
            0 | 1 => {
                let got = pool.reserve();
                let want = m.lowest_free();
                let got_index = got.as_ref().map(index);
                log.push_str(&format!("reserve->{got_index:?}; "));
                assert_eq!(got_index, want, "seed {seed}: reserve. {log}");
                if let Some(s) = got {
                    m.held.insert(index(&s), s);
                }
            }
            2 => {
                if !m.held.is_empty() && r.below(4) != 0 {
                    let keys: Vec<u32> = m.held.keys().copied().collect();
                    let k = keys[r.below(keys.len() as u64) as usize];
                    let s = m.held.remove(&k).unwrap();
                    log.push_str(&format!("release {k}; "));
                    pool.release(&s);
                    m.released.push(s);
                } else if let Some(s) = m
                    .released
                    .last()
                    .filter(|s| !m.held.contains_key(&index(s)))
                {
                    // A second release of a block nobody holds now changes nothing.
                    log.push_str(&format!("release {} again; ", index(s)));
                    pool.release(s);
                }
            }
            3 => {
                let taken: Vec<Ipv4Cidr> =
                    (0..1 + r.below(2)).map(|_| random_taken(&mut r)).collect();
                let before = m.foreign.clone();
                for c in &taken {
                    m.foreign.extend(blocks_of(c));
                }
                let n = pool.exclude(&taken);
                let want = m.foreign.difference(&before).count();
                log.push_str(&format!("exclude {taken:?}->{n}; "));
                assert!(
                    n >= want,
                    "seed {seed}: exclude counted {n} < {want}. {log}"
                );
            }
            _ => {
                let taken: Vec<Ipv4Cidr> =
                    (0..1 + r.below(3)).map(|_| random_taken(&mut r)).collect();
                let mut keep: Vec<Subnet> = m
                    .held
                    .values()
                    .filter(|_| r.below(3) != 0)
                    .cloned()
                    .collect();
                if let Some(s) = m.released.last()
                    && r.below(3) == 0
                {
                    // A stale handle in the keep set must not protect a free block.
                    keep.push(s.clone());
                }
                let kept: BTreeSet<u32> = keep
                    .iter()
                    .map(index)
                    .filter(|k| m.held.contains_key(k))
                    .collect();
                let before = m.foreign.clone();
                for c in &taken {
                    let covered = blocks_of(c);
                    // A /29 or smaller inside one kept block is that run's own network.
                    if c.prefix() >= 29 && covered.len() == 1 && kept.contains(&covered[0]) {
                        continue;
                    }
                    m.foreign.extend(covered);
                }
                let n = pool.exclude_except(&taken, &keep);
                let want = m.foreign.difference(&before).count();
                log.push_str(&format!(
                    "exclude_except {taken:?} keep {:?}->{n}; ",
                    keep.iter().map(index).collect::<Vec<_>>()
                ));
                assert!(
                    n >= want,
                    "seed {seed}: exclude_except counted {n} < {want}. {log}"
                );
            }
        }
        assert_eq!(
            pool.available(),
            m.available(),
            "seed {seed}: available. {log}"
        );
        assert_eq!(pool.in_use(), m.in_use(), "seed {seed}: in_use. {log}");
        assert_eq!(pool.capacity(), BLOCKS as usize, "seed {seed}: capacity");
    }
}

#[test]
fn the_pool_agrees_with_the_model_over_random_operations() {
    for seed in 0..SEEDS {
        run_seed(seed);
    }
}
