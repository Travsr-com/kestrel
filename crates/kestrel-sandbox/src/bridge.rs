//! The gateway bridge: a small `socat` container that lets sandboxes on `--internal` networks
//! reach the gateway running on the host, and nothing else. It sits on the shared `kestrel-gw`
//! network, and drivers built `with_bridge` also join it to each run's own network.

use std::net::Ipv4Addr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use url::Url;

use crate::{CommandRunner, Ipv4Cidr, SandboxError, Subnet, SubnetPool, check_image};

/// Pinned `socat` image used when [`BridgeSpec::image`] is not set.
pub const DEFAULT_BRIDGE_IMAGE: &str =
    "alpine/socat:1.8.0.3@sha256:beb4a68d9e4fe6b0f21ea774a0fde6c31f580dde6368939ed70100c5385b015e";
/// Prefix of the bridge container name, followed by `-<instance id>`. Sandboxes reach the
/// container by IP, not by name.
pub const BRIDGE_NAME: &str = "kestrel-gateway";
/// The `--internal` network the sandboxes join.
const NETWORK: &str = "kestrel-gw";
/// Prefix of the span [`GatewayBridge::plan_network`] claims for `kestrel-gw`: a `/24`, 32 blocks
/// of the subnet pool and room for 253 containers, so the bridges of many `kestrel serve`
/// instances fit on one docker daemon (and, with `gateway.advertise_url`, many sandboxes).
pub const BRIDGE_NETWORK_PREFIX: u8 = 24;
/// What docker says when a network has no address left for one more container.
const NETWORK_FULL: &str = "no available IPv4 addresses";
/// Connections the bridge holds at once when [`BridgeSpec::connections`] is not set: the size
/// `kestrel serve` works out for its default settings (2 for each of 256 waiting calls plus 4
/// for each of 4 concurrent runs).
pub const DEFAULT_BRIDGE_CONNECTIONS: u32 = 528;
/// Most connections a bridge may hold at once.
pub const MAX_BRIDGE_CONNECTIONS: u32 = 8192;
/// Processes allowed on top of one per connection: `socat` itself (PID 1) and spare, so the
/// container's process limit never trips before `socat` stops taking connections.
const SPARE_PIDS: u32 = 8;
/// Memory for `socat` itself, and for each connection (one forked `socat` each).
const BASE_MEMORY_MIB: u32 = 32;
const MEMORY_PER_CONNECTION_MIB: u32 = 1;
/// `-T`: seconds without data in either direction before `socat` closes a connection. It is an
/// inactivity timeout, so a model stream that is silent for more than five minutes is cut.
const IDLE_TIMEOUT_SECS: u32 = 300;

/// Docker's container name charset: `[a-zA-Z0-9][a-zA-Z0-9_.-]*`.
pub(crate) fn is_container_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// How to build the bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeSpec {
    /// A `socat` image whose entrypoint is `socat`. Defaults to [`DEFAULT_BRIDGE_IMAGE`].
    pub image: String,
    /// Start the container with `--add-host host.docker.internal:host-gateway`. True (the
    /// default) when the docker daemon runs on the same machine as Kestrel. False when it runs in
    /// a VM, which already resolves `host.docker.internal` to the host (issue #53).
    pub add_host_gateway: bool,
    /// Connections the bridge holds at once (`socat max-children`), 1 to
    /// [`MAX_BRIDGE_CONNECTIONS`]. Defaults to [`DEFAULT_BRIDGE_CONNECTIONS`]. A connection past
    /// it waits for one to close. The container's process and memory limits grow with it.
    pub connections: u32,
    /// Where the shared `kestrel-gw` network comes from. `kestrel serve` sets it from
    /// [`GatewayBridge::plan_network`]. Defaults to [`BridgeNetwork::DockerPools`].
    pub network: BridgeNetwork,
}

impl Default for BridgeSpec {
    fn default() -> Self {
        Self {
            image: DEFAULT_BRIDGE_IMAGE.to_string(),
            add_host_gateway: true,
            connections: DEFAULT_BRIDGE_CONNECTIONS,
            network: BridgeNetwork::DockerPools,
        }
    }
}

/// Where the bridge's shared `kestrel-gw` network comes from (issue #99).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum BridgeNetwork {
    /// Create it when missing, on a subnet docker picks from its default address pools.
    #[default]
    DockerPools,
    /// It exists already: use it as it is, whatever its subnet, and do not create it. If another
    /// serve removes it before the bridge joins it, it is created once on `fallback` (the span
    /// the plan would have given it, not taken from the plan: a run network that later overlaps
    /// it is refused by docker and the engine moves on to another block).
    Existing { fallback: Option<Subnet> },
    /// Create it on this span (`--subnet`, `--gateway`). One made by someone else in the
    /// meantime is used as it is, and one removed again before the bridge joins it is made once
    /// more.
    Subnet(Subnet),
}

impl BridgeNetwork {
    /// The span to create `kestrel-gw` on when it is gone at `connect`.
    fn span(&self) -> Option<&Subnet> {
        match self {
            BridgeNetwork::DockerPools => None,
            BridgeNetwork::Existing { fallback } => fallback.as_ref(),
            BridgeNetwork::Subnet(span) => Some(span),
        }
    }
}

/// `docker network create --internal --subnet <span> --gateway <first host> kestrel-gw`.
fn create_on(span: &Subnet) -> Vec<String> {
    argv(&[
        "docker",
        "network",
        "create",
        "--internal",
        "--subnet",
        &span.cidr().to_string(),
        "--gateway",
        &span.gateway().to_string(),
        NETWORK,
    ])
}

/// Creates `kestrel-gw` with `create`; one that exists already is fine.
async fn create_network(runner: &dyn CommandRunner, create: &[String]) -> Result<(), SandboxError> {
    match docker(runner, create).await {
        Err(e) if !e.contains("already exists") => Err(step("create-network")(e)),
        _ => Ok(()),
    }
}

/// True when docker says `kestrel-gw` does not exist.
fn network_gone(e: &str) -> bool {
    e.contains(NETWORK) && (e.contains("not found") || e.contains("No such network"))
}

/// What [`GatewayBridge::plan_network`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkPlan {
    pub network: BridgeNetwork,
    /// Set when an existing `kestrel-gw` is smaller than the span Kestrel would give it and could
    /// not be recreated. Says how to recreate it by hand.
    pub warning: Option<String>,
}

/// The first IPv4 subnet of `kestrel-gw`, from `{{range .IPAM.Config}}{{.Subnet}} {{end}}`.
/// `None` when there is none.
fn parse_gw(answer: &str) -> Option<Ipv4Cidr> {
    answer
        .split_whitespace()
        .find_map(|s| Ipv4Cidr::parse(s).ok())
}

/// The `connect` failure, explained when `kestrel-gw` has no address left.
fn connect_error(e: String) -> SandboxError {
    if !e.contains(NETWORK_FULL) {
        return step("connect")(e);
    }
    step("connect")(format!(
        "{NETWORK} is full: every address of its subnet is taken, most likely by the bridges of other kestrel serve instances on this docker daemon or by leaked kestrel-gateway-* containers. List them with `docker ps -a --filter label=kestrel.managed=bridge` and remove the ones whose serve is no longer running (`docker rm --force <name>`). To make {NETWORK} larger, stop every kestrel serve on this daemon and run `docker network rm {NETWORK}`: the next serve creates it on a /{BRIDGE_NETWORK_PREFIX} of gateway.subnet_pool. Docker said: {e}"
    ))
}

/// A running bridge container. Not published on any host port.
pub struct GatewayBridge {
    runner: Arc<dyn CommandRunner>,
    url: Url,
    name: String,
    stopped: AtomicBool,
}

impl std::fmt::Debug for GatewayBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayBridge")
            .field("url", &self.url.as_str())
            .finish_non_exhaustive()
    }
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| (*p).to_string()).collect()
}

fn remove_argv(name: &str) -> Vec<String> {
    argv(&["docker", "rm", "--force", name])
}

/// Removes `name`; a container that is already gone is fine.
async fn remove(runner: &dyn CommandRunner, name: &str) -> Result<(), String> {
    match docker(runner, &remove_argv(name)).await {
        Err(e) if !e.contains("No such container") => Err(e),
        _ => Ok(()),
    }
}

/// Runs one docker command and returns its stdout. `Err` carries the text for [`SandboxError::Bridge`].
async fn docker(runner: &dyn CommandRunner, argv: &[String]) -> Result<String, String> {
    match runner.run(argv).await {
        Ok(out) if out.status == 0 => Ok(out.stdout),
        Ok(out) => Err(out.stderr.trim().to_string()),
        Err(e) => Err(e.to_string()),
    }
}

fn step(name: &'static str) -> impl Fn(String) -> SandboxError {
    move |stderr| SandboxError::Bridge { step: name, stderr }
}

/// `http://<ip>:<port>/` from the `inspect` answer, which must be exactly one IPv4 address.
fn bridge_url(answer: &str, port: u16) -> Result<Url, SandboxError> {
    let bad = |what: String| step("inspect")(what);
    let ip: Ipv4Addr = answer
        .trim()
        .parse()
        .map_err(|_| bad("the bridge has no IPv4 address on kestrel-gw".to_string()))?;
    Url::parse(&format!("http://{ip}:{port}/")).map_err(|e| bad(e.to_string()))
}

impl GatewayBridge {
    /// Replaces any stale bridge and starts a new one forwarding to `host_port` on the host.
    ///
    /// # Errors
    /// Input errors before any command runs; [`SandboxError::Bridge`] when a step fails (the
    /// half-made container is removed first).
    pub async fn start(
        runner: Arc<dyn CommandRunner>,
        host_port: u16,
        spec: BridgeSpec,
        db: &Path,
    ) -> Result<GatewayBridge, SandboxError> {
        if host_port == 0 {
            return Err(SandboxError::InvalidBridgePort);
        }
        let connections = spec.connections;
        if !(1..=MAX_BRIDGE_CONNECTIONS).contains(&connections) {
            return Err(SandboxError::InvalidBridgeConnections { connections });
        }
        check_image(&spec.image)?;

        let id = Self::instance_id(db);
        let name = Self::container_name(&id);
        let managed = "kestrel.managed=bridge";
        let db_label = format!("kestrel.db={id}");
        // Only containers carrying both labels for this database are ever removed.
        let list = argv(&[
            "docker",
            "ps",
            "-a",
            "--filter",
            &format!("label={managed}"),
            "--filter",
            &format!("label={db_label}"),
            "--format",
            "{{.Names}}",
        ]);
        let stale = docker(&*runner, &list)
            .await
            .map_err(step("remove-stale"))?;
        for stale in stale.lines().map(str::trim).filter(|l| !l.is_empty()) {
            // Names are data from docker: never let one become an option of `docker rm`.
            if !is_container_name(stale) {
                eprintln!("kestrel: ignoring unexpected container name from docker ps");
                continue;
            }
            remove(&*runner, stale)
                .await
                .map_err(step("remove-stale"))?;
        }
        let create = match &spec.network {
            BridgeNetwork::DockerPools => Some(argv(&[
                "docker",
                "network",
                "create",
                "--internal",
                NETWORK,
            ])),
            BridgeNetwork::Existing { .. } => None,
            BridgeNetwork::Subnet(span) => Some(create_on(span)),
        };
        if let Some(create) = create {
            create_network(&*runner, &create).await?;
        }

        let listen = format!("TCP-LISTEN:{host_port},fork,reuseaddr,max-children={connections}");
        let pids = (connections + SPARE_PIDS).to_string();
        let memory = format!(
            "{}m",
            BASE_MEMORY_MIB + connections * MEMORY_PER_CONNECTION_MIB
        );
        let target = format!("TCP:host.docker.internal:{host_port}");
        let mut run = argv(&[
            "docker",
            "run",
            "--detach",
            "--name",
            &name,
            "--label",
            managed,
            "--label",
            &db_label,
            "--network",
            "bridge",
        ]);
        if spec.add_host_gateway {
            run.extend(argv(&["--add-host", "host.docker.internal:host-gateway"]));
        }
        run.extend(argv(&[
            "--restart",
            "no",
            "--cap-drop",
            "ALL",
            "--read-only",
            "--security-opt",
            "no-new-privileges",
            "--pids-limit",
            &pids,
            "--memory",
            &memory,
            "--user",
            "65534:65534",
            &spec.image,
            "-T",
            &IDLE_TIMEOUT_SECS.to_string(),
            &listen,
            &target,
        ]));
        let connect = argv(&["docker", "network", "connect", NETWORK, &name]);
        // The name is not used by sandboxes: Docker's DNS does not answer inside gVisor, so they
        // reach the bridge by its IP on the internal network.
        let inspect = argv(&[
            "docker",
            "inspect",
            "--format",
            &format!("{{{{(index .NetworkSettings.Networks \"{NETWORK}\").IPAddress}}}}"),
            &name,
        ]);
        if let Err(e) = docker(&*runner, &run).await {
            if e.contains("already in use") {
                // Someone else owns the name: do not remove it, and say why.
                return Err(step("run")(format!(
                    "the container name {name} is taken by a container without the label {managed} for this database, so Kestrel will not remove it. Remove it yourself or use another database path. Docker said: {e}"
                )));
            }
            let _ = docker(&*runner, &remove_argv(&name)).await;
            return Err(step("run")(e));
        }
        let started = async {
            let first = docker(&*runner, &connect).await;
            // Another serve removed kestrel-gw since the plan: make it once on the span, then join.
            let remake = match &first {
                Err(e) if network_gone(e) => spec.network.span(),
                _ => None,
            };
            if let Some(span) = remake {
                create_network(&*runner, &create_on(span)).await?;
                docker(&*runner, &connect).await.map_err(connect_error)?;
            } else {
                first.map_err(connect_error)?;
            }
            let answer = docker(&*runner, &inspect).await.map_err(step("inspect"))?;
            bridge_url(&answer, host_port)
        }
        .await;
        match started {
            Ok(url) => Ok(GatewayBridge {
                runner,
                url,
                name,
                stopped: AtomicBool::new(false),
            }),
            Err(e) => {
                // Best effort: the original error is the one worth reporting.
                let _ = docker(&*runner, &remove_argv(&name)).await;
                Err(e)
            }
        }
    }

    /// Decides where `kestrel-gw` comes from, before [`GatewayBridge::start`].
    ///
    /// * Missing: it gets an aligned span of `plan` ([`BridgeNetwork::Subnet`]), a
    ///   `/`[`BRIDGE_NETWORK_PREFIX`] unless the pool cannot hold that plus one run block (see
    ///   [`SubnetPool::claim_span`]). The span is taken out of the plan for good, so no run network
    ///   ever gets a block of it.
    /// * Existing and at least as large as that span, or unreadable: reused as it is
    ///   ([`BridgeNetwork::Existing`]), taking nothing from `plan`. The span it would get is kept
    ///   as the fallback for a `kestrel-gw` that vanishes before the bridge joins it.
    /// * Existing and smaller (a `/29` left by an older build): removed and recreated on the span
    ///   when no container is on it (`docker ps -a`, so created and stopped ones count too).
    ///   Otherwise kept, with a warning that says how to recreate it.
    ///
    /// # Errors
    /// [`SandboxError::Bridge`] (step `create-network`) when docker cannot list or read its
    /// networks, or when `kestrel-gw` is missing and `plan` cannot hold one `/29` for it plus one
    /// for runs. Nothing is taken from `plan` then.
    pub async fn plan_network(
        runner: &dyn CommandRunner,
        plan: &SubnetPool,
    ) -> Result<NetworkPlan, SandboxError> {
        let name = format!("name=^{NETWORK}$");
        let lookup = argv(&[
            "docker",
            "network",
            "ls",
            "--filter",
            &name,
            "--format",
            "{{.Name}}",
        ]);
        let listed = docker(runner, &lookup)
            .await
            .map_err(step("create-network"))?;
        // The filter is a pattern: only the exact name counts.
        if listed.lines().any(|l| l.trim() == NETWORK) {
            return Self::existing_network(runner, plan).await;
        }
        Self::claim_network(plan)
    }

    /// `kestrel-gw` on a span claimed from `plan`, or the error naming `gateway.subnet_pool`.
    fn claim_network(plan: &SubnetPool) -> Result<NetworkPlan, SandboxError> {
        let free = plan.available();
        let span = plan.claim_span(BRIDGE_NETWORK_PREFIX).ok_or_else(|| {
            step("create-network")(format!(
                "subnet pool {} has {free} free /29 block(s), but the {NETWORK} network needs one and runs need at least one more. Set gateway.subnet_pool to a larger or free range",
                plan.pool()
            ))
        })?;
        Ok(NetworkPlan {
            network: BridgeNetwork::Subnet(span),
            warning: None,
        })
    }

    /// The plan for an existing `kestrel-gw`: kept, or recreated on a larger span of `plan`.
    async fn existing_network(
        runner: &dyn CommandRunner,
        plan: &SubnetPool,
    ) -> Result<NetworkPlan, SandboxError> {
        let fallback = plan.claimable(BRIDGE_NETWORK_PREFIX);
        let keep = |warning: Option<String>| {
            Ok(NetworkPlan {
                network: BridgeNetwork::Existing {
                    fallback: fallback.clone(),
                },
                warning,
            })
        };
        let inspect = argv(&[
            "docker",
            "network",
            "inspect",
            "--format",
            "{{range .IPAM.Config}}{{.Subnet}} {{end}}",
            NETWORK,
        ]);
        let answer = docker(runner, &inspect)
            .await
            .map_err(step("create-network"))?;
        let Some(current) = parse_gw(&answer) else {
            return keep(None);
        };
        let Some(span) = fallback
            .as_ref()
            .filter(|span| span.cidr().prefix() < current.prefix())
        else {
            return keep(None);
        };
        // `.Containers` of the network lists only running endpoints: a created or stopped
        // container on kestrel-gw (a sandbox between create and start) would be missed.
        let filter = format!("network={NETWORK}");
        let list = argv(&["docker", "ps", "-aq", "--filter", &filter]);
        let attached = docker(runner, &list)
            .await
            .map_err(step("create-network"))?
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count();
        let room = (1_u64 << (32 - u32::from(current.prefix()))).saturating_sub(3);
        let smaller = format!(
            "{NETWORK} is {current}, room for {room} containers, smaller than the {} Kestrel would give it",
            span.cidr()
        );
        let how = format!(
            "To make it larger, stop every kestrel serve on this docker daemon, run `docker network rm {NETWORK}`, then start serve again"
        );
        if attached > 0 {
            return keep(Some(format!(
                "{smaller}, and {attached} container(s) are attached, so Kestrel keeps it. {how}"
            )));
        }
        if let Err(e) = docker(runner, &argv(&["docker", "network", "rm", NETWORK])).await {
            return keep(Some(format!(
                "{smaller}, and removing it failed, so Kestrel keeps it: {e}. {how}"
            )));
        }
        Self::claim_network(plan)
    }

    /// The 8 hex chars naming the bridge of the database at `db`: the first 8 of the 16-digit
    /// lower-case hex FNV-1a 64 hash of the absolute path (`std::path::absolute`, no symlink
    /// resolution) as bytes. Used in the container name and the `kestrel.db` label.
    #[must_use]
    pub fn instance_id(db: &Path) -> String {
        let path = std::path::absolute(db).unwrap_or_else(|_| db.to_path_buf());
        let hash = path
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
                (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
            });
        format!("{hash:016x}")[..8].to_string()
    }

    /// `kestrel-gateway-<id>`.
    #[must_use]
    pub fn container_name(id: &str) -> String {
        format!("{BRIDGE_NAME}-{id}")
    }

    /// Removes the container. Idempotent; failures are ignored (the next `start` replaces it).
    pub async fn stop(&self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Err(e) = remove(&*self.runner, &self.name).await {
            eprintln!("kestrel: gateway bridge stop failed: {e}");
        }
    }

    /// `http://<bridge ip>:<host_port>/`, the URL sandboxes use.
    #[must_use]
    pub fn url(&self) -> Url {
        self.url.clone()
    }

    /// The container name, which drivers join to every run network (`with_bridge`).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pids_limit_leaves_room_for_socat_and_all_its_children() {
        const { assert!(SPARE_PIDS >= 2) };
        // The largest limit still fits in a u32 with its spare processes and memory.
        const {
            assert!(MAX_BRIDGE_CONNECTIONS.checked_add(SPARE_PIDS).is_some());
            assert!(
                BASE_MEMORY_MIB
                    .checked_add(MAX_BRIDGE_CONNECTIONS * MEMORY_PER_CONNECTION_MIB)
                    .is_some()
            );
        };
    }

    #[test]
    fn container_names_follow_dockers_charset() {
        for ok in ["a", "kestrel-gateway-b29b95f5", "a.b_c-d", "0x"] {
            assert!(is_container_name(ok), "{ok}");
        }
        for bad in ["", "-f", "--volumes", "a b", "_x", ".x", "a/b", "a\u{e9}"] {
            assert!(!is_container_name(bad), "{bad}");
        }
    }
}
