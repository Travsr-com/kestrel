//! The gateway bridge: a small `socat` container that lets sandboxes on the `--internal`
//! `kestrel-gw` network reach the gateway running on the host, and nothing else.
//!
//! Skeleton only (tests first, see issue #8): every method is `todo!()`.

#![allow(clippy::todo, unused_variables)]

use std::sync::Arc;

use url::Url;

use crate::{CommandRunner, SandboxError};

/// Pinned `socat` image used when [`BridgeSpec::image`] is not set.
pub const DEFAULT_BRIDGE_IMAGE: &str = "alpine/socat:1.8.0.3";
/// Name of the bridge container, and the host name sandboxes use to reach it.
pub const BRIDGE_NAME: &str = "kestrel-gateway";

/// How to build the bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeSpec {
    /// A `socat` image whose entrypoint is `socat`. Defaults to [`DEFAULT_BRIDGE_IMAGE`].
    pub image: String,
}

impl Default for BridgeSpec {
    fn default() -> Self {
        todo!()
    }
}

/// A running bridge container. Not published on any host port.
#[derive(Debug)]
pub struct GatewayBridge {}

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
        todo!()
    }

    /// Removes the container. Idempotent; failures are ignored (the next `start` replaces it).
    pub async fn stop(&self) {
        todo!()
    }

    /// `http://kestrel-gateway:<host_port>/`, the URL sandboxes use.
    #[must_use]
    pub fn url(&self) -> Url {
        todo!()
    }
}
