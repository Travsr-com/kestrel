//! The gateway bridge: a small `socat` container that lets sandboxes on the `--internal`
//! `kestrel-gw` network reach the gateway running on the host, and nothing else.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use url::Url;

use crate::{CommandRunner, SandboxError, check_image};

/// Pinned `socat` image used when [`BridgeSpec::image`] is not set.
pub const DEFAULT_BRIDGE_IMAGE: &str = "alpine/socat:1.8.0.3";
/// Name of the bridge container, and the host name sandboxes use to reach it.
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

/// Runs one docker command. `Err` carries the text for [`SandboxError::Bridge`].
async fn docker(runner: &dyn CommandRunner, argv: &[String]) -> Result<(), String> {
    match runner.run(argv).await {
        Ok(out) if out.status == 0 => Ok(()),
        Ok(out) => Err(out.stderr.trim().to_string()),
        Err(e) => Err(e.to_string()),
    }
}

fn step(name: &'static str) -> impl Fn(String) -> SandboxError {
    move |stderr| SandboxError::Bridge { step: name, stderr }
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
    ) -> Result<GatewayBridge, SandboxError> {
        if host_port == 0 {
            return Err(SandboxError::InvalidBridgePort);
        }
        check_image(&spec.image)?;
        let url = Url::parse(&format!("http://{BRIDGE_NAME}:{host_port}/"))
            .map_err(|_| SandboxError::InvalidBridgePort)?;

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
        let started = match docker(&*runner, &run).await {
            Ok(()) => docker(&*runner, &connect).await.map_err(step("connect")),
            Err(e) => Err(step("run")(e)),
        };
        if let Err(e) = started {
            // Best effort: the original error is the one worth reporting.
            let _ = docker(&*runner, &remove_argv()).await;
            return Err(e);
        }
        Ok(GatewayBridge {
            runner,
            url,
            stopped: AtomicBool::new(false),
        })
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

    /// `http://kestrel-gateway:<host_port>/`, the URL sandboxes use.
    #[must_use]
    pub fn url(&self) -> Url {
        self.url.clone()
    }
}
