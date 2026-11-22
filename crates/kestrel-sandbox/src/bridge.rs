//! The gateway bridge: a small `socat` container that lets sandboxes on `--internal` networks
//! reach the gateway running on the host, and nothing else. It sits on the shared `kestrel-gw`
//! network, and drivers built `with_bridge` also join it to each run's own network.

use std::net::Ipv4Addr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use url::Url;

use crate::{CommandRunner, SandboxError, check_image};

/// Pinned `socat` image used when [`BridgeSpec::image`] is not set.
pub const DEFAULT_BRIDGE_IMAGE: &str =
    "alpine/socat:1.8.0.3@sha256:beb4a68d9e4fe6b0f21ea774a0fde6c31f580dde6368939ed70100c5385b015e";
/// Prefix of the bridge container name, followed by `-<instance id>`. Sandboxes reach the
/// container by IP, not by name.
pub const BRIDGE_NAME: &str = "kestrel-gateway";
/// The `--internal` network the sandboxes join.
const NETWORK: &str = "kestrel-gw";
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
}

impl Default for BridgeSpec {
    fn default() -> Self {
        Self {
            image: DEFAULT_BRIDGE_IMAGE.to_string(),
            add_host_gateway: true,
            connections: DEFAULT_BRIDGE_CONNECTIONS,
        }
    }
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
        let network = argv(&["docker", "network", "create", "--internal", NETWORK]);
        match docker(&*runner, &network).await {
            Err(e) if !e.contains("already exists") => return Err(step("create-network")(e)),
            _ => {}
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
            docker(&*runner, &connect).await.map_err(step("connect"))?;
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
