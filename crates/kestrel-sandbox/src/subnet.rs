//! Kestrel's own subnet plan for per-run networks (issue #34).
//!
//! Every run network gets a `/29` block from one pool, by default `10.200.0.0/16`, so a fan-out
//! never depends on docker's default address pools. Blocks that overlap an existing docker
//! network or a host route are excluded before any is handed out.

use std::collections::{BTreeSet, HashSet};
use std::net::Ipv4Addr;
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::{
    CommandRunner, INSTANCE_LABEL, NETWORK_GONE, RUN_LABEL, SandboxError, check_id,
    is_container_name,
};

/// The pool used when `gateway.subnet_pool` is not set.
pub const DEFAULT_SUBNET_POOL: &str = "10.200.0.0/16";
/// Prefix length of one block: 8 addresses, room for docker's gateway, the bridge and the
/// sandbox.
pub const SUBNET_BLOCK_PREFIX: u8 = 29;

/// Addresses in one block.
const BLOCK_SIZE: u32 = 1 << (32 - SUBNET_BLOCK_PREFIX);

/// Errors from parsing or checking a subnet pool.
#[derive(Debug, thiserror::Error)]
pub enum SubnetError {
    /// The text is not an IPv4 CIDR.
    #[error("`{text}` is not an IPv4 CIDR like 10.200.0.0/16")]
    Parse { text: String },
    /// The address has bits set past the prefix.
    #[error("`{text}` has host bits set; use the network address `{network}`")]
    HostBits { text: String, network: Ipv4Cidr },
    /// The pool cannot hold one block.
    #[error("`{pool}` is smaller than one block; the pool must be at least a /29")]
    TooSmall { pool: Ipv4Cidr },
}

/// An IPv4 block, `addr/prefix`, with `addr` the network address (host bits zero).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Ipv4Cidr {
    addr: Ipv4Addr,
    prefix: u8,
}

impl Ipv4Cidr {
    /// Parses `a.b.c.d/n`. Refuses anything else, a prefix above 32 and host bits set.
    pub fn parse(text: &str) -> Result<Ipv4Cidr, SubnetError> {
        let (addr, prefix) = split(text).ok_or_else(|| SubnetError::Parse {
            text: text.to_string(),
        })?;
        let cidr = Ipv4Cidr::masked(addr, prefix);
        if cidr.addr != addr {
            return Err(SubnetError::HostBits {
                text: text.to_string(),
                network: cidr,
            });
        }
        Ok(cidr)
    }

    /// Like [`Ipv4Cidr::parse`], with host bits cleared instead of refused. For what docker and
    /// `ip route` print, where only the covered range matters.
    fn parse_lenient(text: &str) -> Option<Ipv4Cidr> {
        split(text).map(|(addr, prefix)| Ipv4Cidr::masked(addr, prefix))
    }

    /// `addr/prefix` with the host bits of `addr` cleared. `prefix` is at most 32.
    fn masked(addr: Ipv4Addr, prefix: u8) -> Ipv4Cidr {
        let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
        Ipv4Cidr {
            addr: Ipv4Addr::from(u32::from(addr) & mask),
            prefix,
        }
    }

    pub fn addr(&self) -> Ipv4Addr {
        self.addr
    }

    pub fn prefix(&self) -> u8 {
        self.prefix
    }

    /// First and last address, as numbers.
    fn range(&self) -> (u64, u64) {
        let start = u64::from(u32::from(self.addr));
        (start, start + (1_u64 << (32 - self.prefix)) - 1)
    }

    /// True when the two blocks share at least one address.
    pub fn overlaps(&self, other: &Ipv4Cidr) -> bool {
        let (a, b) = (self.range(), other.range());
        a.0 <= b.1 && b.0 <= a.1
    }
}

/// `a.b.c.d/n` split into its parts, with `n` one or two digits and at most 32.
fn split(text: &str) -> Option<(Ipv4Addr, u8)> {
    let (addr, prefix) = text.split_once('/')?;
    let digits =
        !prefix.is_empty() && prefix.len() <= 2 && prefix.bytes().all(|b| b.is_ascii_digit());
    let prefix: u8 = prefix.parse().ok().filter(|p| digits && *p <= 32)?;
    Some((addr.parse().ok()?, prefix))
}

impl std::fmt::Display for Ipv4Cidr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

/// One reserved `/29` block for one run's network.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Subnet {
    block: Ipv4Cidr,
}

impl Subnet {
    pub fn cidr(&self) -> Ipv4Cidr {
        self.block
    }

    /// Docker's gateway address on the block: its first host address.
    pub fn gateway(&self) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.block.addr).wrapping_add(1))
    }
}

/// Hands out the `/29` blocks of one pool, lowest address first. Safe to share between threads
/// (`Arc<SubnetPool>`): every method takes `&self` and one lock serialises them.
pub struct SubnetPool {
    pool: Ipv4Cidr,
    /// Blocks in the pool.
    blocks: u32,
    state: Mutex<PoolState>,
}

#[derive(Default)]
struct PoolState {
    /// Block indices never to hand out, as sorted, disjoint, non-touching `[start, end)` ranges.
    excluded: Vec<(u32, u32)>,
    /// Block indices handed out and not yet released.
    used: BTreeSet<u32>,
}

impl PoolState {
    /// The excluded range holding `block`, if any.
    fn excluded_range(&self, block: u32) -> Option<(u32, u32)> {
        let after = self.excluded.partition_point(|(start, _)| *start <= block);
        after
            .checked_sub(1)
            .and_then(|i| self.excluded.get(i))
            .filter(|(_, end)| block < *end)
            .copied()
    }

    fn exclude(&mut self, start: u32, end: u32) {
        self.excluded.push((start, end));
        self.excluded.sort_unstable();
        let mut merged: Vec<(u32, u32)> = Vec::with_capacity(self.excluded.len());
        for (start, end) in self.excluded.drain(..) {
            match merged.last_mut() {
                Some(last) if start <= last.1 => last.1 = last.1.max(end),
                _ => merged.push((start, end)),
            }
        }
        self.excluded = merged;
    }
}

impl std::fmt::Debug for SubnetPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubnetPool")
            .field("pool", &self.pool)
            .field("available", &self.available())
            .finish()
    }
}

impl SubnetPool {
    /// A pool of every `/29` block in `pool`, all free. `pool` must be a `/29` or larger.
    pub fn new(pool: Ipv4Cidr) -> Result<SubnetPool, SubnetError> {
        if pool.prefix > SUBNET_BLOCK_PREFIX {
            return Err(SubnetError::TooSmall { pool });
        }
        Ok(SubnetPool {
            pool,
            blocks: 1 << (SUBNET_BLOCK_PREFIX - pool.prefix),
            state: Mutex::default(),
        })
    }

    /// Parses and checks a pool setting such as `10.200.0.0/16`.
    pub fn parse(text: &str) -> Result<SubnetPool, SubnetError> {
        SubnetPool::new(Ipv4Cidr::parse(text)?)
    }

    pub fn pool(&self) -> Ipv4Cidr {
        self.pool
    }

    /// Blocks in the pool, free or not.
    pub fn capacity(&self) -> usize {
        usize::try_from(self.blocks).unwrap_or(usize::MAX)
    }

    fn state(&self) -> MutexGuard<'_, PoolState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Never hands out a block that overlaps any of `taken` (docker networks, host routes, a
    /// block docker refused). A block in use stays in use until released, and is not handed out
    /// again after that.
    pub fn exclude(&self, taken: &[Ipv4Cidr]) {
        let (first, last) = self.pool.range();
        let mut state = self.state();
        for t in taken {
            let (start, end) = t.range();
            if end < first || start > last {
                continue;
            }
            let from = (start.max(first) - first) / u64::from(BLOCK_SIZE);
            let to = (end.min(last) - first) / u64::from(BLOCK_SIZE) + 1;
            // Both fit: the pool has at most 2^29 blocks.
            if let (Ok(from), Ok(to)) = (u32::try_from(from), u32::try_from(to)) {
                state.exclude(from, to);
            }
        }
    }

    /// Blocks that can still be handed out.
    pub fn available(&self) -> usize {
        let state = self.state();
        let excluded: u64 = state
            .excluded
            .iter()
            .map(|(start, end)| u64::from(end - start))
            .sum();
        let used = state
            .used
            .iter()
            .filter(|b| state.excluded_range(**b).is_none())
            .count() as u64;
        usize::try_from(u64::from(self.blocks) - excluded - used).unwrap_or(usize::MAX)
    }

    /// Reserves the lowest free block, or `None` when every block is in use or excluded.
    pub fn reserve(&self) -> Option<Subnet> {
        let mut state = self.state();
        let mut block = 0;
        while block < self.blocks {
            if let Some((_, end)) = state.excluded_range(block) {
                block = end;
            } else if state.used.contains(&block) {
                block += 1;
            } else {
                // Inside the pool, so it fits: the pool's last address is at most 2^32 - 1.
                let offset = block.wrapping_mul(BLOCK_SIZE);
                state.used.insert(block);
                return Some(Subnet {
                    block: Ipv4Cidr {
                        addr: Ipv4Addr::from(u32::from(self.pool.addr).wrapping_add(offset)),
                        prefix: SUBNET_BLOCK_PREFIX,
                    },
                });
            }
        }
        None
    }

    /// Gives a block back. Releasing twice, or a block of another pool, changes nothing.
    pub fn release(&self, subnet: &Subnet) {
        let (start, _) = subnet.block.range();
        let (first, last) = self.pool.range();
        if subnet.block.prefix != SUBNET_BLOCK_PREFIX || start < first || start > last {
            return;
        }
        if let Ok(block) = u32::try_from((start - first) / u64::from(BLOCK_SIZE)) {
            self.state().used.remove(&block);
        }
    }
}

/// The IPv4 subnets docker networks already use: `docker network ls -q`, then one
/// `docker network inspect` for their IPAM subnets. IPv6 subnets are ignored. A network removed
/// between the two calls is skipped.
pub async fn docker_subnets(runner: &dyn CommandRunner) -> Result<Vec<Ipv4Cidr>, SandboxError> {
    docker_subnets_except_instance(runner, None).await
}

/// Like [`docker_subnets`], but leaves out the run networks of Kestrel instance `instance`: the
/// ones labelled [`RUN_LABEL`] and [`INSTANCE_LABEL`]`=<instance>`. Those are this instance's
/// own leftovers (a crash or Ctrl-C), which `Engine::recover` removes before any block is
/// reserved, so they must not shrink the plan. Networks of other instances and of users still
/// count. An `instance` docker could read as an option is refused before any command runs.
pub async fn docker_subnets_except_instance(
    runner: &dyn CommandRunner,
    instance: Option<&str>,
) -> Result<Vec<Ipv4Cidr>, SandboxError> {
    let instance = instance.map(check_id).transpose()?;
    let ids = docker(runner, &["network", "ls", "-q"], &[]).await?;
    let ours: HashSet<String> = match instance {
        Some(id) => {
            let run = format!("label={RUN_LABEL}");
            let mine = format!("label={INSTANCE_LABEL}={id}");
            let args = ["network", "ls", "-q", "--filter", &run, "--filter", &mine];
            docker(runner, &args, &[])
                .await?
                .lines()
                .map(|id| id.trim().to_string())
                .collect()
        }
        None => HashSet::new(),
    };
    let mut inspect = vec![
        "network",
        "inspect",
        "--format",
        "{{range .IPAM.Config}}{{.Subnet}} {{end}}",
    ];
    let before = inspect.len();
    inspect.extend(
        ids.lines()
            .map(str::trim)
            .filter(|id| is_container_name(id) && !ours.contains(*id)),
    );
    if inspect.len() == before {
        return Ok(Vec::new());
    }
    let subnets = docker(runner, &inspect, &NETWORK_GONE).await?;
    Ok(subnets
        .split_whitespace()
        .filter_map(Ipv4Cidr::parse_lenient)
        .collect())
}

/// Runs `docker <args>` and returns its stdout. A failure whose stderr holds one of `tolerated`
/// still returns the stdout.
async fn docker(
    runner: &dyn CommandRunner,
    args: &[&str],
    tolerated: &[&str],
) -> Result<String, SandboxError> {
    // Any docker CLI does: only its program name is used here.
    let cli = &crate::DOCKER_RUNC;
    cli.run_tolerating(runner, &cli.argv(args), tolerated).await
}

/// Route types `ip route` may print before the destination.
const ROUTE_TYPES: [&str; 9] = [
    "unicast",
    "local",
    "broadcast",
    "multicast",
    "throw",
    "unreachable",
    "prohibit",
    "blackhole",
    "nat",
];

/// The IPv4 routes of this host, from `ip -4 route show` (Linux). Without a working `ip` command
/// (another OS, or not installed) the list is empty. `default` routes are ignored.
pub async fn host_routes(runner: &dyn CommandRunner) -> Vec<Ipv4Cidr> {
    let argv: Vec<String> = ["ip", "-4", "route", "show"]
        .iter()
        .map(|p| (*p).to_string())
        .collect();
    let stdout = match runner.run(&argv).await {
        Ok(out) if out.status == 0 => out.stdout,
        _ => return Vec::new(),
    };
    stdout
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let mut dest = words.next()?;
            if ROUTE_TYPES.contains(&dest) {
                dest = words.next()?;
            }
            if dest.contains('/') {
                Ipv4Cidr::parse_lenient(dest)
            } else {
                dest.parse().ok().map(|addr| Ipv4Cidr { addr, prefix: 32 })
            }
        })
        .collect()
}
