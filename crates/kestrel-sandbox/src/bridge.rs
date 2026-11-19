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
/// Name of the bridge container. Sandboxes reach it by IP, not by this name.
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

fn remove_argv() -> Vec<String> {
    argv(&["docker", "rm", "--force", BRIDGE_NAME])
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
        let _ = db; // skeleton (#29): the implementation derives the name and labels from it.
        if host_port == 0 {
            return Err(SandboxError::InvalidBridgePort);
        }
        check_image(&spec.image)?;

        match docker(&*runner, &remove_argv()).await {
            Err(e) if !e.contains("No such container") => return Err(step("remove-stale")(e)),
            _ => {}
        }
        let network = argv(&["docker", "network", "create", "--internal", NETWORK]);
        match docker(&*runner, &network).await {
            Err(e) if !e.contains("already exists") => return Err(step("create-network")(e)),
            _ => {}
        }

        let listen = format!("TCP-LISTEN:{host_port},fork,reuseaddr");
        let target = format!("TCP:host.docker.internal:{host_port}");
        let run = argv(&[
            "docker",
            "run",
            "--detach",
            "--name",
            BRIDGE_NAME,
            "--network",
            "bridge",
            "--add-host",
            "host.docker.internal:host-gateway",
            "--restart",
            "no",
            &spec.image,
            &listen,
            &target,
        ]);
        let connect = argv(&["docker", "network", "connect", NETWORK, BRIDGE_NAME]);
        // The name is not used by sandboxes: Docker's DNS does not answer inside gVisor, so they
        // reach the bridge by its IP on the internal network.
        let inspect = argv(&[
            "docker",
            "inspect",
            "--format",
            &format!("{{{{(index .NetworkSettings.Networks \"{NETWORK}\").IPAddress}}}}"),
            BRIDGE_NAME,
        ]);
        let started = async {
            docker(&*runner, &run).await.map_err(step("run"))?;
            docker(&*runner, &connect).await.map_err(step("connect"))?;
            let answer = docker(&*runner, &inspect).await.map_err(step("inspect"))?;
            bridge_url(&answer, host_port)
        }
        .await;
        match started {
            Ok(url) => Ok(GatewayBridge {
                runner,
                url,
                stopped: AtomicBool::new(false),
            }),
            Err(e) => {
                // Best effort: the original error is the one worth reporting.
                let _ = docker(&*runner, &remove_argv()).await;
                Err(e)
            }
        }
    }

    /// The 8 hex chars naming the bridge of the database at `db`: the first 8 of the 16-digit
    /// lower-case hex FNV-1a 64 hash of the absolute path (`std::path::absolute`, no symlink
    /// resolution) as bytes. Used in the container name and the `kestrel.db` label.
    #[must_use]
    pub fn instance_id(db: &Path) -> String {
        let _ = db;
        todo!("#29: FNV-1a 64 of the absolute database path, first 8 hex chars")
    }

    /// `kestrel-gateway-<id>`.
    #[must_use]
    pub fn container_name(id: &str) -> String {
        let _ = id;
        todo!("#29")
    }

    /// Removes the container. Idempotent; failures are ignored (the next `start` replaces it).
    pub async fn stop(&self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Err(e) = docker(&*self.runner, &remove_argv()).await
            && !e.contains("No such container")
        {
            eprintln!("kestrel: gateway bridge stop failed: {e}");
        }
    }

    /// `http://<bridge ip>:<host_port>/`, the URL sandboxes use.
    #[must_use]
    pub fn url(&self) -> Url {
        self.url.clone()
    }
}
