//! Issue #34: Kestrel's own subnet plan. `Ipv4Cidr` parsing and overlap, the `/29` block pool
//! (`SubnetPool`), and the discovery of subnets that are already taken (`docker_subnets`,
//! `host_routes`) through a fake `CommandRunner`.
//!
//! Scenarios covered here: 2 (an existing docker network is skipped), 3 at pool level (a host route
//! covering the whole pool leaves no block), 6 at pool level (parallel reserves never hand out the
//! same block), 9 (invalid pool settings are refused with a clear message).
//!
//! The fake answers every `docker network inspect` call with one line per network id named in the
//! argv, in argv order, each line holding that network's IPAM subnets separated by spaces. A
//! network with no IPAM config gets an empty line. Parse the answer with `split_whitespace`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::collections::{BTreeMap, HashSet};
use std::net::Ipv4Addr;
use std::sync::{Arc, Barrier};

use common::FakeRunner;
use kestrel_sandbox::{
    CommandOutput, CommandRunner, DEFAULT_SUBNET_POOL, Ipv4Cidr, SUBNET_BLOCK_PREFIX,
    SpawnedProcess, Subnet, SubnetPool, docker_subnets, host_routes,
};

fn cidr(text: &str) -> Ipv4Cidr {
    Ipv4Cidr::parse(text).expect("valid cidr")
}

fn ip(text: &str) -> Ipv4Addr {
    text.parse().expect("ipv4")
}

fn texts(blocks: &[Subnet]) -> Vec<String> {
    blocks.iter().map(|s| s.cidr().to_string()).collect()
}

// Ipv4Cidr ----------------------------------------------------------------------------------------

#[test]
fn a_cidr_parses_into_its_address_and_prefix_and_prints_back() {
    for (text, addr, prefix) in [
        ("10.200.0.0/16", "10.200.0.0", 16),
        ("10.200.0.8/29", "10.200.0.8", 29),
        ("0.0.0.0/0", "0.0.0.0", 0),
        ("192.168.1.7/32", "192.168.1.7", 32),
        ("172.16.0.0/12", "172.16.0.0", 12),
    ] {
        let c = cidr(text);
        assert_eq!(c.addr(), ip(addr), "{text}");
        assert_eq!(c.prefix(), prefix, "{text}");
        assert_eq!(c.to_string(), text, "Display is the docker form");
    }
}

#[test]
fn anything_that_is_not_an_ipv4_cidr_is_refused() {
    for bad in [
        "",
        "banana",
        "10.200.0.0",
        "10.200.0.0/",
        "/16",
        "10.200.0/16",
        "256.0.0.0/8",
        "10.200.0.0/33",
        "10.200.0.0/-1",
        "10.200.0.0/16/1",
        "10.200.0.0/x",
        "fd00::/8",
    ] {
        assert!(Ipv4Cidr::parse(bad).is_err(), "{bad:?} must be refused");
    }
}

#[test]
fn a_cidr_with_host_bits_set_is_refused_and_says_so() {
    let err = Ipv4Cidr::parse("10.200.0.1/16").expect_err("host bits set");
    let msg = err.to_string();
    assert!(msg.to_lowercase().contains("host bits"), "{msg}");
    assert!(Ipv4Cidr::parse("10.200.0.4/29").is_err());
}

#[test]
fn overlap_is_symmetric_and_covers_containment_and_equality() {
    let pairs = [
        ("10.200.0.0/16", "10.200.0.8/29", true),
        ("10.0.0.0/8", "10.200.0.0/16", true),
        ("10.200.0.0/29", "10.200.0.0/29", true),
        ("0.0.0.0/0", "192.168.1.0/24", true),
        ("10.200.0.0/29", "10.200.0.8/29", false),
        ("10.200.0.0/16", "10.201.0.0/16", false),
        ("172.17.0.0/16", "10.200.0.0/16", false),
        ("10.200.0.0/30", "10.200.0.4/30", false),
        ("10.200.0.0/29", "10.200.0.4/30", true),
    ];
    for (a, b, want) in pairs {
        assert_eq!(cidr(a).overlaps(&cidr(b)), want, "{a} vs {b}");
        assert_eq!(cidr(b).overlaps(&cidr(a)), want, "{b} vs {a}");
    }
}

#[test]
fn the_defaults_are_a_slash_16_cut_into_slash_29_blocks() {
    assert_eq!(DEFAULT_SUBNET_POOL, "10.200.0.0/16");
    assert_eq!(SUBNET_BLOCK_PREFIX, 29);
    let pool = SubnetPool::parse(DEFAULT_SUBNET_POOL).expect("default pool");
    assert_eq!(pool.pool(), cidr("10.200.0.0/16"));
    assert_eq!(pool.available(), 8192);
}

// SubnetPool: validation (scenario 9) -------------------------------------------------------------

/// The error text of a refused pool. (`SubnetPool` need not be `Debug`, so no `expect_err`.)
fn refused(made: Result<SubnetPool, kestrel_sandbox::SubnetError>, what: &str) -> String {
    match made {
        Ok(_) => panic!("{what} must be refused"),
        Err(e) => e.to_string(),
    }
}

#[test]
fn s9_a_pool_that_is_not_cidr_is_refused_naming_the_value() {
    for bad in ["banana", "10.200.0.0", "10.200.0.0/33", "10.200.0/16"] {
        let msg = refused(SubnetPool::parse(bad), bad);
        assert!(msg.contains(bad), "{bad}: {msg}");
    }
    assert!(SubnetPool::parse("").is_err());
}

#[test]
fn s9_a_pool_with_host_bits_set_is_refused() {
    let msg = refused(SubnetPool::parse("10.200.0.1/16"), "host bits");
    assert!(msg.contains("10.200.0.1/16"), "{msg}");
    assert!(msg.to_lowercase().contains("host bits"), "{msg}");
}

#[test]
fn s9_a_pool_smaller_than_one_block_is_refused_saying_it_must_be_at_least_a_slash_29() {
    for small in ["10.200.0.0/30", "10.200.0.0/31", "10.200.0.0/32"] {
        let msg = refused(SubnetPool::parse(small), small);
        assert!(msg.contains(small), "{small}: {msg}");
        assert!(msg.contains("/29"), "{small}: {msg}");
        let msg = refused(SubnetPool::new(cidr(small)), small);
        assert!(msg.contains(small), "{small}: {msg}");
        assert!(msg.contains("/29"), "{small}: {msg}");
    }
}

#[test]
fn a_pool_of_exactly_one_block_is_fine() {
    let pool = SubnetPool::parse("10.200.0.0/29").expect("one block");
    assert_eq!(pool.available(), 1);
    let pool = SubnetPool::new(cidr("10.200.0.0/28")).expect("two blocks");
    assert_eq!(pool.available(), 2);
    assert_eq!(pool.pool(), cidr("10.200.0.0/28"));
}

// SubnetPool: reserve and release -----------------------------------------------------------------

#[test]
fn blocks_are_handed_out_lowest_first_with_the_first_host_as_gateway() {
    let pool = SubnetPool::parse(DEFAULT_SUBNET_POOL).expect("pool");
    let a = pool.reserve().expect("a");
    let b = pool.reserve().expect("b");
    let c = pool.reserve().expect("c");
    assert_eq!(
        texts(&[a.clone(), b.clone(), c.clone()]),
        vec!["10.200.0.0/29", "10.200.0.8/29", "10.200.0.16/29"]
    );
    assert_eq!(a.gateway(), ip("10.200.0.1"));
    assert_eq!(b.gateway(), ip("10.200.0.9"));
    assert_eq!(c.gateway(), ip("10.200.0.17"));
    assert_eq!(a.cidr().prefix(), SUBNET_BLOCK_PREFIX);
    assert_eq!(pool.available(), 8192 - 3);
    assert_ne!(a, b);
}

#[test]
fn an_exhausted_pool_reserves_nothing() {
    let pool = SubnetPool::parse("10.200.0.0/28").expect("pool");
    assert!(pool.reserve().is_some());
    assert!(pool.reserve().is_some());
    assert_eq!(pool.available(), 0);
    assert!(pool.reserve().is_none());
    assert!(pool.reserve().is_none(), "still nothing");
}

#[test]
fn a_released_block_is_the_next_one_handed_out() {
    let pool = SubnetPool::parse("10.200.0.0/27").expect("four blocks");
    let a = pool.reserve().expect("a");
    let _b = pool.reserve().expect("b");
    pool.release(&a);
    assert_eq!(pool.available(), 3);
    let again = pool.reserve().expect("lowest free");
    assert_eq!(again.cidr(), a.cidr());
}

#[test]
fn releasing_twice_is_a_no_op() {
    let pool = SubnetPool::parse("10.200.0.0/28").expect("pool");
    let a = pool.reserve().expect("a");
    let b = pool.reserve().expect("b");
    pool.release(&a);
    pool.release(&a);
    assert_eq!(pool.available(), 1, "a second release frees nothing more");
    assert_eq!(pool.reserve().map(|s| s.cidr()), Some(a.cidr()));
    assert!(pool.reserve().is_none(), "b is still in use");
    pool.release(&b);
    pool.release(&b);
    assert_eq!(pool.available(), 1);
}

#[test]
fn releasing_a_block_of_another_pool_is_a_no_op() {
    let pool = SubnetPool::parse("10.200.0.0/28").expect("pool");
    let other = SubnetPool::parse("10.201.0.0/28").expect("other");
    let _a = pool.reserve().expect("a");
    let _b = pool.reserve().expect("b");
    let stranger = other.reserve().expect("stranger");
    pool.release(&stranger);
    assert_eq!(pool.available(), 0);
    assert!(pool.reserve().is_none());
}

// SubnetPool: exclude (scenarios 2 and 3) ---------------------------------------------------------

#[test]
fn s2_an_existing_network_on_the_first_block_is_skipped() {
    let pool = SubnetPool::parse(DEFAULT_SUBNET_POOL).expect("pool");
    pool.exclude(&[cidr("10.200.0.0/29")]);
    assert_eq!(pool.available(), 8191);
    let first = pool.reserve().expect("a block");
    assert_eq!(first.cidr().to_string(), "10.200.0.8/29");
}

#[test]
fn a_taken_range_excludes_every_block_it_touches_and_nothing_else() {
    let pool = SubnetPool::parse(DEFAULT_SUBNET_POOL).expect("pool");
    // Touches only part of the first block, which is enough to skip it.
    pool.exclude(&[cidr("10.200.0.4/30")]);
    // The lower half of the pool.
    pool.exclude(&[cidr("10.200.0.0/17")]);
    // Outside the pool: no effect.
    pool.exclude(&[cidr("172.17.0.0/16"), cidr("192.168.0.0/20")]);
    assert_eq!(pool.available(), 4096);
    assert_eq!(
        pool.reserve().map(|s| s.cidr().to_string()),
        Some("10.200.128.0/29".to_string())
    );
}

#[test]
fn exclusions_add_up() {
    let pool = SubnetPool::parse("10.200.0.0/27").expect("four blocks");
    pool.exclude(&[cidr("10.200.0.0/29")]);
    pool.exclude(&[cidr("10.200.0.16/29")]);
    pool.exclude(&[]);
    assert_eq!(pool.available(), 2);
    let got: Vec<String> = std::iter::from_fn(|| pool.reserve())
        .map(|s| s.cidr().to_string())
        .collect();
    assert_eq!(got, vec!["10.200.0.8/29", "10.200.0.24/29"]);
}

#[test]
fn s3_a_route_covering_the_whole_pool_leaves_no_block() {
    let pool = SubnetPool::parse(DEFAULT_SUBNET_POOL).expect("pool");
    pool.exclude(&[cidr("10.0.0.0/8")]);
    assert_eq!(pool.available(), 0);
    assert!(pool.reserve().is_none());

    let pool = SubnetPool::parse(DEFAULT_SUBNET_POOL).expect("pool");
    pool.exclude(&[cidr("0.0.0.0/0")]);
    assert_eq!(pool.available(), 0);
}

#[test]
fn a_block_excluded_while_reserved_is_never_handed_out_again() {
    // The engine excludes a block docker says is taken ("Pool overlaps") and gives it back.
    let pool = SubnetPool::parse("10.200.0.0/28").expect("pool");
    let a = pool.reserve().expect("a");
    pool.exclude(&[a.cidr()]);
    pool.release(&a);
    assert_eq!(pool.available(), 1);
    assert_eq!(
        pool.reserve().map(|s| s.cidr().to_string()),
        Some("10.200.0.8/29".to_string())
    );
    assert!(pool.reserve().is_none());
}

// SubnetPool: concurrency (scenario 6 at pool level) ----------------------------------------------

#[test]
fn the_pool_is_shareable_across_threads() {
    fn shareable<T: Send + Sync + 'static>() {}
    shareable::<SubnetPool>();
    shareable::<Subnet>();
    shareable::<Ipv4Cidr>();
}

#[test]
fn s6_eight_threads_reserving_at_once_get_eight_distinct_blocks() {
    let pool = Arc::new(SubnetPool::parse(DEFAULT_SUBNET_POOL).expect("pool"));
    let start = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let pool = pool.clone();
            let start = start.clone();
            std::thread::spawn(move || {
                start.wait();
                pool.reserve().expect("a free block")
            })
        })
        .collect();
    let got: Vec<Subnet> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let distinct: HashSet<Ipv4Cidr> = got.iter().map(Subnet::cidr).collect();
    assert_eq!(distinct.len(), 8, "{:?}", texts(&got));
    assert!(
        distinct.iter().all(|c| c.overlaps(&cidr("10.200.0.0/26"))),
        "the eight lowest blocks: {:?}",
        texts(&got)
    );
    assert_eq!(pool.available(), 8192 - 8);
}

#[test]
fn s6_many_threads_reserving_and_releasing_never_share_a_block() {
    // 1024 blocks, 16 threads keeping 32 each and giving 16 back: never short of a block.
    let pool = Arc::new(SubnetPool::parse("10.200.0.0/19").expect("pool"));
    let start = Arc::new(Barrier::new(16));
    let handles: Vec<_> = (0..16)
        .map(|n| {
            let pool = pool.clone();
            let start = start.clone();
            std::thread::spawn(move || {
                start.wait();
                let mut kept = Vec::new();
                for i in 0..48 {
                    let s = pool.reserve().expect("a free block");
                    // Give a third back right away, so releases race with reserves.
                    if (i + n) % 3 == 0 {
                        pool.release(&s);
                    } else {
                        kept.push(s);
                    }
                }
                kept
            })
        })
        .collect();
    let kept: Vec<Subnet> = handles
        .into_iter()
        .flat_map(|h| h.join().unwrap())
        .collect();
    let distinct: HashSet<Ipv4Cidr> = kept.iter().map(Subnet::cidr).collect();
    assert_eq!(distinct.len(), kept.len(), "a block was handed out twice");
    assert_eq!(kept.len(), 16 * 32);
    assert_eq!(pool.available(), 1024 - kept.len());
}

// Discovery: docker_subnets ------------------------------------------------------------------------

/// A runner that plays a docker daemon with the given networks (id to IPAM subnets).
fn daemon(networks: &[(&str, &[&str])]) -> Arc<FakeRunner> {
    let nets: BTreeMap<String, Vec<String>> = networks
        .iter()
        .map(|(id, subnets)| {
            (
                (*id).to_string(),
                subnets.iter().map(|s| (*s).to_string()).collect(),
            )
        })
        .collect();
    FakeRunner::with(move |argv| {
        let words: Vec<&str> = argv.iter().map(String::as_str).collect();
        if words.starts_with(&["docker", "network", "ls"]) {
            let ids: String = nets.keys().map(|id| format!("{id}\n")).collect();
            return out(&ids);
        }
        if words.starts_with(&["docker", "network", "inspect"]) {
            let lines: String = argv
                .iter()
                .filter_map(|a| nets.get(a))
                .map(|subnets| format!("{}\n", subnets.join(" ")))
                .collect();
            return out(&lines);
        }
        failed("unexpected command")
    })
}

fn out(stdout: &str) -> CommandOutput {
    CommandOutput {
        status: 0,
        stdout: stdout.to_string(),
        stderr: String::new(),
    }
}

fn failed(stderr: &str) -> CommandOutput {
    CommandOutput {
        status: 1,
        stdout: String::new(),
        stderr: stderr.to_string(),
    }
}

fn sorted(mut v: Vec<Ipv4Cidr>) -> Vec<Ipv4Cidr> {
    v.sort();
    v
}

#[tokio::test]
async fn docker_subnets_lists_the_ipv4_subnets_of_every_network() {
    let runner = daemon(&[
        ("1a2b3c4d5e6f", &["172.17.0.0/16"]),
        ("2b3c4d5e6f70", &["10.200.0.0/29", "fd00:dead:beef::/64"]),
        ("3c4d5e6f7081", &[]),
        ("4d5e6f708192", &["192.168.16.0/20"]),
    ]);
    let got = docker_subnets(runner.as_ref()).await.expect("subnets");
    assert_eq!(
        sorted(got),
        sorted(vec![
            cidr("172.17.0.0/16"),
            cidr("10.200.0.0/29"),
            cidr("192.168.16.0/20"),
        ]),
        "IPv6 and empty IPAM configs are ignored"
    );
    let calls = runner.runs();
    assert!(
        calls.iter().any(
            |a| a.starts_with(&["docker".into(), "network".into(), "ls".into()])
                && a.contains(&"-q".to_string())
        ),
        "{calls:?}"
    );
}

#[tokio::test]
async fn s2_the_pool_skips_a_block_that_docker_already_uses() {
    let runner = daemon(&[("1a2b3c4d5e6f", &["10.200.0.0/29"])]);
    let pool = SubnetPool::parse(DEFAULT_SUBNET_POOL).expect("pool");
    pool.exclude(&docker_subnets(runner.as_ref()).await.expect("subnets"));
    assert_eq!(
        pool.reserve().map(|s| s.cidr().to_string()),
        Some("10.200.0.8/29".to_string())
    );
}

#[tokio::test]
async fn docker_subnets_with_no_networks_is_empty() {
    let runner = daemon(&[]);
    let got = docker_subnets(runner.as_ref()).await.expect("subnets");
    assert!(got.is_empty());
}

#[tokio::test]
async fn docker_subnets_fails_when_docker_does() {
    let runner = FakeRunner::with(|_| failed("Cannot connect to the Docker daemon"));
    assert!(docker_subnets(runner.as_ref()).await.is_err());
}

// Discovery: host_routes ---------------------------------------------------------------------------

const ROUTES: &str = "\
default via 192.168.1.1 dev eth0 proto dhcp src 192.168.1.20 metric 100
10.200.0.0/16 dev wg0 scope link
172.17.0.0/16 dev docker0 proto kernel scope link src 172.17.0.1 linkdown
192.168.1.0/24 dev eth0 proto kernel scope link src 192.168.1.20 metric 100
";

#[tokio::test]
async fn host_routes_reads_the_ipv4_routes_and_ignores_the_default_route() {
    let runner = FakeRunner::with(|argv| {
        if argv.first().map(String::as_str) == Some("ip") {
            out(ROUTES)
        } else {
            failed("unexpected command")
        }
    });
    let got = host_routes(runner.as_ref()).await;
    if runner.runs().is_empty() {
        // Allowed off Linux: the host has no `ip` routes to read.
        if cfg!(target_os = "linux") {
            panic!("Linux must read the routes with `ip`");
        }
        assert!(got.is_empty());
        return;
    }
    assert_eq!(
        sorted(got),
        sorted(vec![
            cidr("10.200.0.0/16"),
            cidr("172.17.0.0/16"),
            cidr("192.168.1.0/24"),
        ])
    );
    let argv = &runner.runs()[0];
    assert!(argv.contains(&"-4".to_string()), "IPv4 only: {argv:?}");
    assert!(argv.contains(&"route".to_string()), "{argv:?}");
}

#[tokio::test]
async fn s3_a_host_route_over_the_whole_pool_leaves_no_block() {
    let runner = FakeRunner::with(|_| out("10.0.0.0/8 dev tun0 scope link\n"));
    let pool = SubnetPool::parse(DEFAULT_SUBNET_POOL).expect("pool");
    pool.exclude(&host_routes(runner.as_ref()).await);
    if !runner.runs().is_empty() {
        assert_eq!(pool.available(), 0);
        assert!(pool.reserve().is_none());
    }
}

#[tokio::test]
async fn a_failing_ip_command_gives_no_routes() {
    let runner = FakeRunner::with(|_| failed("ip: command not found"));
    assert!(host_routes(runner.as_ref()).await.is_empty());
}

/// A runner whose processes can never start (no `ip` binary at all).
struct Missing;

#[async_trait::async_trait]
impl CommandRunner for Missing {
    async fn run(&self, _: &[String]) -> std::io::Result<CommandOutput> {
        Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no ip"))
    }
    async fn spawn(&self, _: &[String]) -> std::io::Result<SpawnedProcess> {
        Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no ip"))
    }
}

#[tokio::test]
async fn a_missing_ip_command_gives_no_routes() {
    assert!(host_routes(&Missing).await.is_empty());
}
