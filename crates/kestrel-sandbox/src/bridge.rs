//! The gateway bridge: a small `socat` container that lets sandboxes on the `--internal`
//! `kestrel-gw` network reach the gateway running on the host, and nothing else.

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

/// How to build the bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeSpec {
    /// A `socat` image whose entrypoint is `socat`. Defaults to [`DEFAULT_BRIDGE_IMAGE`].
    pub image: String,
}

impl Default for BridgeSpec {
    fn default() -> Self {
        Self {
            image: DEFAULT_BRIDGE_IMAGE.to_string(),
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
            remove(&*runner, stale)
                .await
                .map_err(step("remove-stale"))?;
        }
        let network = argv(&["docker", "network", "create", "--internal", NETWORK]);
        match docker(&*runner, &network).await {
            Err(e) if !e.contains("already exists") => return Err(step("create-network")(e)),
            _ => {}
        }

        let listen = format!("TCP-LISTEN:{host_port},fork,reuseaddr,max-children=64");
        let target = format!("TCP:host.docker.internal:{host_port}");
        let run = argv(&[
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
            "--add-host",
            "host.docker.internal:host-gateway",
            "--restart",
            "no",
            "--cap-drop",
            "ALL",
            "--read-only",
            "--security-opt",
            "no-new-privileges",
            "--pids-limit",
            "64",
            "--memory",
            "64m",
            "--user",
            "65534:65534",
            &spec.image,
            "-T",
            "300",
            &listen,
            &target,
        ]);
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
}
