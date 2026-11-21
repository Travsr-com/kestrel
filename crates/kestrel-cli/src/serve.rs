//! `kestrel serve`: the engine and the LLM gateway in one process, until SIGINT.

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use kestrel_core::{AdapterFactory, Engine, Limits, ModelMap, Workspaces, default_models};
use kestrel_gateway::{
    BudgetGuard, GatewayConfig, GatewayDeps, MeterSink, ModelPrice, ProviderConfig, RunResolver,
};
use kestrel_sandbox::{
    BridgeSpec, CommandRunner, DEFAULT_SUBNET_POOL, GatewayBridge, HostInfo, Ipv4Cidr,
    ProcessRunner, SandboxDriver, SubnetPool, detect_with, docker_subnets_except_instance,
    host_routes,
};
use kestrel_store::Store;
use kestrel_types::{EventKind, MicroUsd, ModelCallRecord, RunId, Tier};
use serde::Deserialize;
use serde_json::json;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::commands::say;
use crate::{Failure, Output, ServeArgs, UpArgs, db_path, open, paths};

/// The config file, as written. The raw structs have no `Debug`, so a key can never be printed
/// from them. See `tests/s9_s10_serve.rs` for an example.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    gateway: RawGateway,
    #[serde(default)]
    engine: RawEngine,
    #[serde(default)]
    providers: RawProviders,
    /// Tier name (`cheap`, `mid`, `frontier`) to provider model name.
    #[serde(default)]
    models: BTreeMap<String, String>,
    #[serde(default)]
    prices: BTreeMap<String, RawPrice>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGateway {
    listen: Option<String>,
    /// Kept as a raw value so a wrong type is reported against the key.
    port: Option<toml::Value>,
    /// Kept as a raw value so a wrong type is reported against the key, not as a TOML error.
    advertise_url: Option<toml::Value>,
    /// Kept as a raw value so a wrong type is reported against the key and never echoed.
    docker_host: Option<toml::Value>,
    /// Kept as a raw value so a wrong type is reported against the key.
    subnet_pool: Option<toml::Value>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEngine {
    max_concurrent_runs: Option<usize>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProviders {
    anthropic: Option<RawProvider>,
    openai: Option<RawProvider>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProvider {
    base_url: Option<String>,
    api_key: Option<String>,
}

/// USD per million tokens, as exact decimal strings.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPrice {
    input_per_mtok: String,
    output_per_mtok: String,
    cache_read_per_mtok: String,
    cache_write_per_mtok: String,
}

/// A checked config.
#[derive(Debug)]
struct Config {
    listen: String,
    /// True when the config file sets `gateway.listen`; an explicit value is always respected.
    listen_explicit: bool,
    /// `gateway.port`: the port of the default bind (the docker bridge IP). Default 7070.
    port: Option<u16>,
    /// Where sandboxes reach the gateway, when the operator sets it.
    advertise_url: Option<Url>,
    /// `gateway.docker_host`: where the docker daemon runs. Default `auto`.
    docker_host: DockerHostSetting,
    /// `gateway.subnet_pool`: where run networks get their `/29` blocks. Default
    /// [`DEFAULT_SUBNET_POOL`].
    subnet_pool: Ipv4Cidr,
    limits: Limits,
    models: ModelMap,
    gateway: GatewayConfig,
}

const DEFAULT_LISTEN: &str = "127.0.0.1:7070";
const ANTHROPIC_URL: &str = "https://api.anthropic.com";
const OPENAI_URL: &str = "https://api.openai.com";

/// Reads and checks the config file. Every problem is a usage error naming the bad key.
fn load_config(path: &Path) -> Result<Config, Failure> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Failure::usage(format!("{}: {e}", path.display())))?;
    let raw: RawConfig = toml::from_str(&text).map_err(|e| toml_error(path, &text, &e))?;

    let anthropic = provider("anthropic", raw.providers.anthropic)?;
    let openai = provider("openai", raw.providers.openai)?;
    if anthropic.is_none() && openai.is_none() {
        return Err(Failure::usage(
            "no provider configured: set providers.anthropic.api_key or providers.openai.api_key",
        ));
    }
    // A provider left out gets its public URL and an empty key, so its calls fail upstream
    // with 401. Good enough for M1; the gateway has no "provider disabled" mode yet.
    let unset = |url: &str| -> Result<ProviderConfig, Failure> {
        Ok(ProviderConfig {
            base_url: parse_url("default", url)?,
            api_key: String::new(),
        })
    };
    let anthropic = anthropic.map_or_else(|| unset(ANTHROPIC_URL), Ok)?;
    let openai = openai.map_or_else(|| unset(OPENAI_URL), Ok)?;

    let max = raw.engine.max_concurrent_runs.unwrap_or(4);
    if max == 0 {
        return Err(Failure::usage(
            "engine.max_concurrent_runs must be at least 1",
        ));
    }
    let mut models = default_models();
    for (tier, model) in raw.models {
        let t: Tier = format!("tier:{tier}").parse().map_err(|_| {
            Failure::usage(
                "models: unknown tier name (expected cheap, mid or frontier)".to_string(),
            )
        })?;
        models.insert(t, model);
    }
    let mut prices = HashMap::new();
    for (model, p) in raw.prices {
        let field = |name: &str, v: &str| {
            MicroUsd::parse_decimal(v).ok_or_else(|| {
                Failure::usage(format!(
                    "prices: {name} is not a non-negative USD amount like 3.00"
                ))
            })
        };
        let price = ModelPrice {
            input_per_mtok: field("input_per_mtok", &p.input_per_mtok)?,
            output_per_mtok: field("output_per_mtok", &p.output_per_mtok)?,
            cache_read_per_mtok: field("cache_read_per_mtok", &p.cache_read_per_mtok)?,
            cache_write_per_mtok: field("cache_write_per_mtok", &p.cache_write_per_mtok)?,
        };
        prices.insert(model, price);
    }
    let advertise_url = raw.gateway.advertise_url.map(advertise_url).transpose()?;
    let port = raw.gateway.port.map(gateway_port).transpose()?;
    let docker_host = raw
        .gateway
        .docker_host
        .map(docker_host_setting)
        .transpose()?
        .unwrap_or_default();
    let subnet_pool = subnet_pool(raw.gateway.subnet_pool)?;
    Ok(Config {
        listen_explicit: raw.gateway.listen.is_some(),
        listen: raw.gateway.listen.unwrap_or_else(|| DEFAULT_LISTEN.into()),
        port,
        advertise_url,
        docker_host,
        subnet_pool,
        limits: Limits {
            max_concurrent_runs: max,
            ..Limits::default()
        },
        models,
        gateway: GatewayConfig {
            anthropic,
            openai,
            prices,
        },
    })
}

/// `gateway.port`: an integer from 1 to 65535.
fn gateway_port(value: toml::Value) -> Result<u16, Failure> {
    value
        .as_integer()
        .and_then(|n| u16::try_from(n).ok())
        .filter(|port| *port != 0)
        .ok_or_else(|| Failure::usage("gateway.port must be a whole number from 1 to 65535"))
}

/// `gateway.subnet_pool`: an IPv4 CIDR string of at least one `/29` block, default
/// [`DEFAULT_SUBNET_POOL`]. Anything else is a usage error naming the key.
fn subnet_pool(value: Option<toml::Value>) -> Result<Ipv4Cidr, Failure> {
    let text = match &value {
        None => DEFAULT_SUBNET_POOL,
        Some(value) => value.as_str().ok_or_else(|| {
            Failure::usage("gateway.subnet_pool must be an IPv4 CIDR string like \"10.200.0.0/16\"")
        })?,
    };
    SubnetPool::parse(text)
        .map(|pool| pool.pool())
        .map_err(|e| Failure::usage(format!("gateway.subnet_pool: {e}")))
}

/// The subnet plan for run networks: every `/29` block of `pool` that no docker network uses
/// and, when docker runs on this host, no host route covers (a VM's routes are not this host's,
/// so they are not read). The leftover run networks of `instance` do not count: `recover` removes
/// them before any block is reserved. Writes one line naming the pool and the free block count
/// to `err`. No free block is a runtime error that says to set `gateway.subnet_pool` to a free
/// range.
async fn subnet_plan(
    pool: Ipv4Cidr,
    runner: &dyn CommandRunner,
    host: DockerHost,
    instance: Option<&str>,
    err: &mut dyn std::io::Write,
) -> Result<Arc<SubnetPool>, Failure> {
    let plan =
        SubnetPool::new(pool).map_err(|e| Failure::usage(format!("gateway.subnet_pool: {e}")))?;
    let networks = docker_subnets_except_instance(runner, instance)
        .await
        .map_err(|e| {
            Failure::runtime(format!("listing docker networks for the subnet pool: {e}"))
        })?;
    plan.exclude(&networks);
    if host == DockerHost::Native {
        plan.exclude(&host_routes(runner).await);
    }
    let free = plan.available();
    if free == 0 {
        return Err(Failure::runtime(format!(
            "subnet pool {pool} has no free /29 block: docker networks or host routes use all of it. Set gateway.subnet_pool to a free range"
        )));
    }
    // A closed stderr must not stop serve.
    let _ = writeln!(
        err,
        "networks: one per run, from subnet pool {pool} ({free} free /29 blocks)"
    );
    Ok(Arc::new(plan))
}

/// The warning for sandboxes on the one shared network, which is what they get with docker
/// drivers but no bridge (`gateway.advertise_url` set).
fn shared_network_warning(docker: bool, advertise: Option<&Url>) -> Option<&'static str> {
    (docker && advertise.is_some()).then_some(
        "warning: gateway.advertise_url is set, so there is no gateway bridge and all sandboxes share one network: they can reach each other",
    )
}

/// `gateway.docker_host` as written in the config (issue #53).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum DockerHostSetting {
    /// Decide by trying to bind the docker bridge IP locally.
    #[default]
    Auto,
    /// The docker daemon runs on this machine.
    Native,
    /// The docker daemon runs in a VM (Rancher Desktop, Docker Desktop, Colima).
    Vm,
}

/// Where the docker daemon runs, once `auto` is resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DockerHost {
    Native,
    Vm,
}

/// `gateway.docker_host`: the string `auto`, `native` or `vm`. Anything else is a usage error
/// that names `gateway.docker_host` and never echoes the value.
fn docker_host_setting(value: toml::Value) -> Result<DockerHostSetting, Failure> {
    match value.as_str() {
        Some("auto") => Ok(DockerHostSetting::Auto),
        Some("native") => Ok(DockerHostSetting::Native),
        Some("vm") => Ok(DockerHostSetting::Vm),
        _ => Err(Failure::usage(
            "gateway.docker_host must be \"auto\", \"native\" or \"vm\"",
        )),
    }
}

/// Which mode serve runs in. `bind` is the result of trying to bind `<docker bridge IP>:0`
/// locally. Explicit `native` and `vm` win and ignore it. `auto`: Ok means native,
/// `AddrNotAvailable` means vm, any other error is a runtime failure naming `gateway.listen`.
fn choose_docker_host(
    setting: DockerHostSetting,
    bind: &std::io::Result<()>,
) -> Result<DockerHost, Failure> {
    match (setting, bind) {
        (DockerHostSetting::Native, _) | (DockerHostSetting::Auto, Ok(())) => {
            Ok(DockerHost::Native)
        }
        (DockerHostSetting::Vm, _) => Ok(DockerHost::Vm),
        (DockerHostSetting::Auto, Err(e)) if e.kind() == std::io::ErrorKind::AddrNotAvailable => {
            Ok(DockerHost::Vm)
        }
        (DockerHostSetting::Auto, Err(e)) => Err(Failure::runtime(format!(
            "gateway.listen: cannot tell where docker runs, binding the docker bridge IP failed ({e}). Set gateway.listen, or set gateway.docker_host to \"vm\" if docker runs in a VM"
        ))),
    }
}

/// Like [`check_bridge_listen`] but mode aware. `Native` is exactly `check_bridge_listen`. `Vm`
/// accepts a loopback address (no error, no warning) because the VM reaches the host through
/// loopback; `0.0.0.0` and `::` still get the LAN warning.
fn check_host_listen(
    host: DockerHost,
    bridge: bool,
    addr: SocketAddr,
) -> Result<Option<String>, Failure> {
    if host == DockerHost::Vm && addr.ip().is_loopback() {
        return Ok(None);
    }
    check_bridge_listen(bridge, addr)
}

/// `auto`: asks docker for the bridge IP, then tries to bind it locally.
async fn detect_docker_host(runner: &dyn CommandRunner) -> Result<(DockerHost, Ipv4Addr), Failure> {
    let ip = discover_bridge_ip(runner).await?;
    let probe = TcpListener::bind((ip, 0)).await.map(drop);
    Ok((choose_docker_host(DockerHostSetting::Auto, &probe)?, ip))
}

/// Picks the mode and the gateway bind string, and writes one line to `err` saying which mode
/// was chosen, only when `needs_bridge` is true (`gateway: docker daemon on this host` or
/// `gateway: docker daemon in a VM, binding loopback`).
///
/// * `needs_bridge` false: `Native`, the plain loopback default, no docker call, no line.
/// * `native`: as `resolve_listen` (docker asked once, only without an explicit `listen`).
/// * `vm`: docker never asked; bind is `listen`, else `127.0.0.1:<port or 7070>`.
/// * `auto`: docker asked once for the bridge IP (even with an explicit `listen`), then
///   `choose_docker_host` on the result of binding `<ip>:0`. With an explicit `listen`, any
///   failure of that detection means `Native` and that `listen`, as before #53.
async fn resolve_gateway(
    runner: &dyn CommandRunner,
    setting: DockerHostSetting,
    listen: Option<&str>,
    port: Option<u16>,
    needs_bridge: bool,
    err: &mut dyn std::io::Write,
) -> Result<(DockerHost, String), Failure> {
    if !needs_bridge {
        return Ok((DockerHost::Native, gateway_bind(listen, port, None)));
    }
    let (host, bind) = match setting {
        DockerHostSetting::Vm => (DockerHost::Vm, gateway_bind(listen, port, None)),
        DockerHostSetting::Native => (
            DockerHost::Native,
            resolve_listen(runner, listen, port, true).await?,
        ),
        DockerHostSetting::Auto => match detect_docker_host(runner).await {
            Ok((DockerHost::Native, ip)) => {
                (DockerHost::Native, gateway_bind(listen, port, Some(ip)))
            }
            Ok((DockerHost::Vm, _)) => (DockerHost::Vm, gateway_bind(listen, port, None)),
            // An explicit listen never needed the lookup before, so a failed one is not fatal.
            Err(_) if listen.is_some() => (DockerHost::Native, gateway_bind(listen, port, None)),
            Err(e) => return Err(e),
        },
    };
    let line = match host {
        DockerHost::Native => "gateway: docker daemon on this host",
        DockerHost::Vm => "gateway: docker daemon in a VM, binding loopback",
    };
    // A closed stderr must not stop serve.
    let _ = writeln!(err, "{line}");
    Ok((host, bind))
}

/// Names that may appear in a config error. Anything else quoted in an error came from the
/// user and may be an API key, so it is masked.
const CONFIG_NAMES: &[&str] = &[
    "gateway",
    "listen",
    "port",
    "advertise_url",
    "docker_host",
    "subnet_pool",
    "engine",
    "max_concurrent_runs",
    "providers",
    "anthropic",
    "openai",
    "base_url",
    "api_key",
    "models",
    "cheap",
    "mid",
    "frontier",
    "prices",
    "input_per_mtok",
    "output_per_mtok",
    "cache_read_per_mtok",
    "cache_write_per_mtok",
];

/// A TOML error as `<path>:<line>:<column>: <message>`. Never quotes the source line, which may
/// hold an API key, and masks every quoted part of the message that is not a config name.
fn toml_error(path: &Path, text: &str, e: &toml::de::Error) -> Failure {
    let mut place = String::new();
    if let Some(span) = e.span() {
        let before = text.get(..span.start).unwrap_or_default();
        let line = before.matches('\n').count() + 1;
        let line_start = before.rfind('\n').map_or(0, |i| i + 1);
        let column = span.start - line_start + 1;
        place = format!(":{line}:{column}");
    }
    let message = mask_quoted(e.message().trim()).replace('\n', "; ");
    Failure::usage(format!("{}{place}: {message}", path.display()))
}

/// Replaces the inside of every `` `...` `` and `"..."` segment with `***`, unless it is a known
/// config name or only punctuation (as in "expected `\"`"). An unclosed quote masks the rest.
fn mask_quoted(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(open) = rest.find(['`', '"']) {
        let quote = &rest[open..=open];
        out.push_str(&rest[..=open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find(quote) else {
            out.push_str("***");
            return out;
        };
        let inner = &after[..close];
        let safe = CONFIG_NAMES.contains(&inner)
            || (inner.len() <= 2 && !inner.chars().any(char::is_alphanumeric));
        out.push_str(if safe { inner } else { "***" });
        out.push_str(quote);
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

/// A warning when a config file holding API keys is readable by other users.
#[cfg(unix)]
fn permission_warning(path: &Path) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path).ok()?.permissions().mode() & 0o777;
    (mode & 0o077 != 0).then(|| {
        format!(
            "warning: {} holds API keys but other users can read it (mode {mode:o}); run chmod 600 on it",
            path.display()
        )
    })
}

#[cfg(not(unix))]
fn permission_warning(_path: &Path) -> Option<String> {
    None
}

/// A provider section, if present. Its `api_key` must be set and non-empty; no environment
/// variable fills it in.
fn provider(name: &str, raw: Option<RawProvider>) -> Result<Option<ProviderConfig>, Failure> {
    let Some(raw) = raw else { return Ok(None) };
    let api_key = raw
        .api_key
        .filter(|k| !k.is_empty())
        .ok_or_else(|| Failure::usage(format!("providers.{name}.api_key is missing or empty")))?;
    let default = if name == "anthropic" {
        ANTHROPIC_URL
    } else {
        OPENAI_URL
    };
    let url = raw.base_url.as_deref().unwrap_or(default);
    Ok(Some(ProviderConfig {
        base_url: parse_url(&format!("providers.{name}.base_url"), url)?,
        api_key,
    }))
}

/// `gateway.advertise_url`: an absolute http or https URL. The value is never echoed.
fn advertise_url(value: toml::Value) -> Result<Url, Failure> {
    value
        .as_str()
        .and_then(|text| Url::parse(text).ok())
        .filter(|url| matches!(url.scheme(), "http" | "https") && url.has_host())
        .ok_or_else(|| {
            Failure::usage("gateway.advertise_url: must be an absolute http or https URL")
        })
}

/// The URL sandboxes use to reach the gateway: the operator's `advertise_url`, else the bridge,
/// else the address the gateway bound.
fn gateway_url_for(
    advertise: Option<&Url>,
    bridge: Option<&Url>,
    bound: SocketAddr,
) -> Result<Url, Failure> {
    match (advertise, bridge) {
        (Some(url), _) | (None, Some(url)) => Ok(url.clone()),
        (None, None) => parse_url("gateway.listen", &format!("http://{bound}/")),
    }
}

/// True when a selected driver runs sandboxes in docker, so they need the bridge to reach the
/// gateway. The debug-only fake driver and Apple container do not.
fn uses_docker(drivers: &[Box<dyn SandboxDriver>]) -> bool {
    drivers
        .iter()
        .any(|d| matches!(d.name(), "gvisor" | "docker-runc"))
}

/// The bridge reaches the gateway through the host, so a loopback-only gateway would refuse it
/// and every sandbox model call would fail. `bridge` is true when serve would start the bridge.
///
/// `Ok(Some(warning))` for a bind that exposes the gateway beyond the docker bridge (`0.0.0.0`,
/// `::`); `Ok(None)` when there is nothing to say.
fn check_bridge_listen(bridge: bool, addr: SocketAddr) -> Result<Option<String>, Failure> {
    if !bridge {
        return Ok(None);
    }
    if addr.ip().is_loopback() {
        return Err(Failure::usage(format!(
            "gateway.listen is {addr}, but sandboxes reach the gateway through a container, which a loopback address refuses. Remove gateway.listen to bind the docker bridge IP, set a non-loopback address, or set gateway.advertise_url"
        )));
    }
    Ok(addr.ip().is_unspecified().then(|| {
        format!(
            "warning: gateway.listen is {addr}, so the gateway is reachable from your LAN and only run tokens protect it. Remove gateway.listen to bind the docker bridge IP instead"
        )
    }))
}

/// Port of the default gateway bind.
const DEFAULT_PORT: u16 = 7070;

/// Where the gateway binds. An explicit `gateway.listen` is returned unchanged. Otherwise, with
/// `bridge_ip` (the docker bridge gateway IP), `<bridge_ip>:<port>`; without it,
/// `127.0.0.1:<port>`. `port` is `gateway.port`, default 7070.
fn gateway_bind(listen: Option<&str>, port: Option<u16>, bridge_ip: Option<Ipv4Addr>) -> String {
    if let Some(listen) = listen {
        return listen.to_string();
    }
    let host = bridge_ip.unwrap_or(Ipv4Addr::LOCALHOST);
    format!("{host}:{}", port.unwrap_or(DEFAULT_PORT))
}

/// What to do when the docker bridge IP cannot be found.
const BRIDGE_HINT: &str = "set gateway.listen (for example to your docker bridge IP and a port) to skip the lookup, or set gateway.docker_host to \"vm\" if docker runs in a VM";

/// The one IPv4 address `docker network inspect bridge --format {{(index .IPAM.Config 0).Gateway}}`
/// prints (surrounding whitespace allowed). Anything else is a runtime failure.
fn parse_bridge_ip(stdout: &str) -> Result<Ipv4Addr, Failure> {
    stdout.trim().parse().map_err(|_| {
        Failure::runtime(format!(
            "docker network inspect bridge did not print one IPv4 address for the docker bridge ({:?}); {BRIDGE_HINT}",
            stdout.trim()
        ))
    })
}

/// Asks docker for the bridge network's gateway IP, through `runner`. A non-zero exit or an I/O
/// error is a runtime failure that names `docker network inspect bridge` and `gateway.listen`.
async fn discover_bridge_ip(runner: &dyn CommandRunner) -> Result<Ipv4Addr, Failure> {
    let argv: Vec<String> = [
        "docker",
        "network",
        "inspect",
        "bridge",
        "--format",
        "{{(index .IPAM.Config 0).Gateway}}",
    ]
    .iter()
    .map(|p| (*p).to_string())
    .collect();
    let fail = |why: String| {
        Failure::runtime(format!(
            "cannot find the docker bridge IP: docker network inspect bridge failed ({why}); {BRIDGE_HINT}"
        ))
    };
    let out = runner.run(&argv).await.map_err(|e| fail(e.to_string()))?;
    if out.status != 0 {
        return Err(fail(out.stderr.trim().to_string()));
    }
    parse_bridge_ip(&out.stdout)
}

/// The gateway bind string. Runs docker (once) only when `bridge` is true and `listen` is not
/// set explicitly; otherwise it never touches `runner`.
async fn resolve_listen(
    runner: &dyn CommandRunner,
    listen: Option<&str>,
    port: Option<u16>,
    bridge: bool,
) -> Result<String, Failure> {
    let bridge_ip = if bridge && listen.is_none() {
        Some(discover_bridge_ip(runner).await?)
    } else {
        None
    };
    Ok(gateway_bind(listen, port, bridge_ip))
}

/// The lock file of `db`: the database file name plus `.serve.lock`, in the same directory.
fn serve_lock_path(db: &Path) -> PathBuf {
    let mut name = db.as_os_str().to_os_string();
    name.push(".serve.lock");
    PathBuf::from(name)
}

/// Holds the exclusive lock of one `kestrel serve` per database until dropped (or the process
/// dies). Taken with `std::fs::File::try_lock`, so no `unsafe` is needed.
struct ServeLock {
    _file: std::fs::File,
}

/// Creates `serve_lock_path(db)` if needed and takes the exclusive lock. A second holder gets a
/// runtime failure (exit 1) whose message names the lock file.
fn acquire_serve_lock(db: &Path) -> Result<ServeLock, Failure> {
    let path = serve_lock_path(db);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| Failure::runtime(format!("{}: {e}", path.display())))?;
    file.try_lock().map_err(|e| match e {
        std::fs::TryLockError::WouldBlock => Failure::runtime(format!(
            "another kestrel serve is already using this database (lock file {}). Stop it first",
            path.display()
        )),
        std::fs::TryLockError::Error(e) => Failure::runtime(format!("{}: {e}", path.display())),
    })?;
    Ok(ServeLock { _file: file })
}

/// Starts the bridge when sandboxes need one: docker drivers and no `advertise_url`.
async fn start_bridge(
    runner: Arc<dyn CommandRunner>,
    host: DockerHost,
    docker: bool,
    advertise: Option<&Url>,
    port: u16,
    db: &Path,
) -> Result<Option<GatewayBridge>, Failure> {
    if !docker || advertise.is_some() {
        return Ok(None);
    }
    let spec = BridgeSpec {
        add_host_gateway: host == DockerHost::Native,
        ..BridgeSpec::default()
    };
    GatewayBridge::start(runner, port, spec, db)
        .await
        .map(Some)
        .map_err(|e| Failure::runtime(e.to_string()))
}

fn parse_url(key: &str, url: &str) -> Result<Url, Failure> {
    Url::parse(url).map_err(|_| Failure::usage(format!("{key}: not a valid absolute URL")))
}

/// Debug builds only: `--sandbox-driver fake` swaps the real drivers for the fake one.
#[cfg(debug_assertions)]
fn test_drivers(args: &ServeArgs) -> Result<Option<Vec<Box<dyn SandboxDriver>>>, Failure> {
    match args.sandbox_driver.as_deref() {
        None => Ok(None),
        Some("fake") => Ok(Some(vec![Box::new(crate::fake::FakeDriver::default())])),
        Some(other) => Err(Failure::usage(format!("unknown sandbox driver `{other}`"))),
    }
}

#[cfg(not(debug_assertions))]
fn test_drivers(_args: &ServeArgs) -> Result<Option<Vec<Box<dyn SandboxDriver>>>, Failure> {
    Ok(None)
}

/// Where `kestrel up` puts the API server unless `--listen` says otherwise.
const DEFAULT_API_LISTEN: &str = "127.0.0.1:7171";

/// What `kestrel up` adds to `kestrel serve`: the API server with the UI.
pub(crate) struct ApiOptions {
    listen: Option<String>,
    open: bool,
}

pub(crate) fn serve(db: Option<PathBuf>, args: ServeArgs, out: Output) -> Result<(), Failure> {
    start(db, args, out, None)
}

pub(crate) fn up(db: Option<PathBuf>, args: UpArgs, out: Output) -> Result<(), Failure> {
    let api = ApiOptions {
        listen: args.listen,
        open: !args.no_open,
    };
    start(db, args.serve, out, Some(api))
}

/// The API server's address and a warning when it is not loopback.
fn api_listen(listen: Option<&str>) -> Result<(SocketAddr, Option<String>), Failure> {
    let text = listen.unwrap_or(DEFAULT_API_LISTEN);
    let addr: SocketAddr = text.parse().map_err(|_| {
        Failure::usage(format!(
            "--listen `{text}` is not an address like {DEFAULT_API_LISTEN}"
        ))
    })?;
    let warning = (!addr.ip().is_loopback()).then(|| {
        format!(
            "the API server listens on {addr}, which is not loopback: anyone who can reach it and has the token can read and steer every run"
        )
    });
    Ok((addr, warning))
}

/// The address to open the UI at, with the token for the UI to pick up. An unspecified bind
/// (0.0.0.0 or ::) is opened on loopback.
fn ui_url(addr: SocketAddr, token: &str) -> String {
    let host = match addr.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => {
            SocketAddr::from((Ipv4Addr::LOCALHOST, addr.port()))
        }
        IpAddr::V6(ip) if ip.is_unspecified() => {
            SocketAddr::from((Ipv6Addr::LOCALHOST, addr.port()))
        }
        _ => addr,
    };
    format!("http://{host}/?token={token}")
}

/// The API token's file: next to the database, so a temp `--db` keeps everything together.
fn token_path(db: &Path) -> PathBuf {
    db.with_file_name("server.token")
}

/// The command that opens `url` in the default browser on `os` (`std::env::consts::OS`).
fn browser_command(os: &str, url: &str) -> Option<(String, Vec<String>)> {
    let program = match os {
        "macos" => "open",
        "windows" => "explorer",
        "linux" | "freebsd" | "openbsd" | "netbsd" | "dragonfly" => "xdg-open",
        _ => return None,
    };
    Some((program.to_string(), vec![url.to_string()]))
}

/// Opens `url` in a browser, or says to when that is not possible.
fn open_browser(url: &str) {
    let opened = browser_command(std::env::consts::OS, url).is_some_and(|(program, args)| {
        std::process::Command::new(program)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .is_ok()
    });
    if !opened {
        eprintln!("kestrel: open {url} in a browser");
    }
}

/// The API server, bound and ready to serve.
struct Api {
    addr: SocketAddr,
    url: String,
    open: bool,
    serve: std::pin::Pin<Box<dyn Future<Output = std::io::Result<()>> + Send>>,
}

async fn bind_api(
    store: Arc<Store>,
    db: &Path,
    options: ApiOptions,
    shutdown: CancellationToken,
) -> Result<Api, Failure> {
    let (addr, warning) = api_listen(options.listen.as_deref())?;
    if let Some(warning) = warning {
        eprintln!("{warning}");
    }
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| Failure::runtime(format!("--listen {addr}: cannot listen there: {e}")))?;
    let addr = listener
        .local_addr()
        .map_err(|e| Failure::runtime(e.to_string()))?;
    let path = token_path(db);
    let token = kestrel_server::load_or_create_token(&path)
        .map_err(|e| Failure::runtime(format!("{}: {e}", path.display())))?;
    let app = kestrel_server::router(
        store,
        kestrel_server::Config::new(token.clone(), addr),
        Arc::new(kestrel_server::EmbeddedUi),
        shutdown.clone(),
    );
    Ok(Api {
        addr,
        url: ui_url(addr, &token),
        open: options.open,
        serve: Box::pin(kestrel_server::serve(listener, app, shutdown)),
    })
}

fn start(
    db: Option<PathBuf>,
    args: ServeArgs,
    out: Output,
    api: Option<ApiOptions>,
) -> Result<(), Failure> {
    let path = match &args.config {
        Some(p) => p.clone(),
        None => paths::config_dir()?.join("kestrel").join("config.toml"),
    };
    let config = load_config(&path)?;
    if let Some(warning) = permission_warning(&path) {
        eprintln!("{warning}");
    }
    let drivers = test_drivers(&args)?;
    let rt = tokio::runtime::Runtime::new()
        .map_err(|e| Failure::runtime(format!("starting the async runtime: {e}")))?;
    // One serve per database: the lock is held until this function returns, and before any
    // bridge or socket exists.
    let db = db_path(db)?;
    let _lock = acquire_serve_lock(&db)?;
    rt.block_on(run(db, config, drivers, out, api))
}

async fn run(
    db: PathBuf,
    config: Config,
    drivers: Option<Vec<Box<dyn SandboxDriver>>>,
    out: Output,
    api: Option<ApiOptions>,
) -> Result<(), Failure> {
    // Catch SIGINT before anything else, so Ctrl-C always means a clean stop.
    let shutdown = CancellationToken::new();
    let interrupted = interrupt()?;
    let stop = shutdown.clone();
    tokio::spawn(async move {
        interrupted.await;
        stop.cancel();
    });

    let drivers = match drivers {
        Some(d) => d,
        None => {
            let instance = GatewayBridge::instance_id(&db);
            // The bridge, when serve starts one, has this name: docker drivers join it to every
            // run network. Without a bridge (`advertise_url`) there are no run networks.
            let bridge = GatewayBridge::container_name(&instance);
            let bridge = config.advertise_url.is_none().then_some(bridge.as_str());
            detect_with(&HostInfo::current().await, false, Some(&instance), bridge)
                .map_err(|e| Failure::runtime(e.to_string()))?
        }
    };
    let docker = uses_docker(&drivers);
    let needs_bridge = docker && config.advertise_url.is_none();
    if let Some(warning) = shared_network_warning(docker, config.advertise_url.as_ref()) {
        eprintln!("{warning}");
    }
    let explicit = config.listen_explicit.then_some(config.listen.as_str());
    let (host, listen) = resolve_gateway(
        &ProcessRunner,
        config.docker_host,
        explicit,
        config.port,
        needs_bridge,
        &mut std::io::stderr(),
    )
    .await?;
    let listener = TcpListener::bind(&listen)
        .await
        .map_err(|e| Failure::runtime(format!("gateway.listen: cannot listen there: {e}")))?;
    let addr = listener
        .local_addr()
        .map_err(|e| Failure::runtime(e.to_string()))?;
    // The discovered docker bridge IP is the expected bind, so it is not worth a warning.
    let discovered = needs_bridge && explicit.is_none();
    if let Some(warning) = check_host_listen(host, needs_bridge, addr)? {
        eprintln!("{warning}");
    } else if !discovered && !addr.ip().is_loopback() {
        eprintln!(
            "gateway listens on {addr}, which is not loopback: only run tokens protect it, bind to a private interface if you can"
        );
    }
    // Before the bridge: a pool with no free block stops serve with nothing to clean up.
    let subnets = if needs_bridge {
        let mut err = std::io::stderr();
        // This instance's leftover run networks do not count: `recover` removes them before
        // any block is reserved.
        let instance = GatewayBridge::instance_id(&db);
        let plan = subnet_plan(
            config.subnet_pool,
            &ProcessRunner,
            host,
            Some(&instance),
            &mut err,
        );
        Some(plan.await?)
    } else {
        None
    };
    let bridge = start_bridge(
        Arc::new(ProcessRunner),
        host,
        docker,
        config.advertise_url.as_ref(),
        addr.port(),
        &db,
    )
    .await?;
    let bridge_url = bridge.as_ref().map(GatewayBridge::url);
    let result = match gateway_url_for(config.advertise_url.as_ref(), bridge_url.as_ref(), addr) {
        Ok(gateway_url) => {
            let bound = Bound {
                listener,
                addr,
                gateway_url,
                shutdown,
                subnets,
            };
            serve_bound(&db, config, drivers, bound, out, api).await
        }
        Err(e) => Err(e),
    };
    // Every way out of serving, clean or not, removes the bridge.
    if let Some(bridge) = &bridge {
        bridge.stop().await;
    }
    result
}

/// The gateway socket and what depends on it.
struct Bound {
    listener: TcpListener,
    addr: SocketAddr,
    gateway_url: Url,
    shutdown: CancellationToken,
    /// The subnet plan for run networks, when serve runs the bridge.
    subnets: Option<Arc<SubnetPool>>,
}

async fn serve_bound(
    db: &Path,
    config: Config,
    drivers: Vec<Box<dyn SandboxDriver>>,
    bound: Bound,
    out: Output,
    api: Option<ApiOptions>,
) -> Result<(), Failure> {
    let Bound {
        listener,
        addr,
        gateway_url,
        shutdown,
        subnets,
    } = bound;
    let store = Arc::new(open(Some(db.to_path_buf()))?);
    // Workspaces live next to the database, so a temp `--db` keeps everything in one place.
    let workspace_root = std::path::absolute(db)
        .map_err(|e| Failure::runtime(format!("{}: {e}", db.display())))?
        .parent()
        .map(|d| d.join("workspaces"))
        .ok_or_else(|| Failure::runtime(format!("{}: no parent dir", db.display())))?;

    let adapters: AdapterFactory = Arc::new(kestrel_agent::adapter_for);
    let engine = Engine::with_models(
        store.clone(),
        drivers,
        adapters,
        gateway_url,
        config.limits,
        config.models,
    )
    .with_workspaces(Workspaces::new(workspace_root, Arc::new(ProcessRunner)))
    .with_instance(GatewayBridge::instance_id(db));
    let engine = match subnets {
        Some(pool) => engine.with_subnets(pool),
        None => engine,
    };
    engine
        .recover()
        .await
        .map_err(|e| Failure::runtime(e.to_string()))?;

    let api = match api {
        Some(options) => Some(bind_api(store.clone(), db, options, shutdown.clone()).await?),
        None => None,
    };
    let deps = Arc::new(StoreDeps(store));
    let app = kestrel_gateway::router(
        config.gateway,
        GatewayDeps {
            resolver: deps.clone(),
            budget: deps.clone(),
            meter: deps,
        },
    );
    let gateway =
        axum::serve(listener, app).with_graceful_shutdown(shutdown.clone().cancelled_owned());
    let (shown, serve_api) = match api {
        Some(api) => ((api.addr, api.url, api.open).into(), Some(api.serve)),
        None => (None, None),
    };
    let server = tokio::spawn(async move {
        match serve_api {
            // Both stop on `shutdown`; the first error stops the pair.
            Some(serve_api) => tokio::try_join!(async { gateway.await }, serve_api).map(|_| ()),
            None => gateway.await,
        }
    });

    match (&shown, out.json) {
        (Some((api_addr, url, _)), true) => say(
            &json!({ "gateway": addr.to_string(), "api": api_addr.to_string(), "url": url })
                .to_string(),
        )?,
        (None, true) => say(&json!({ "gateway": addr.to_string() }).to_string())?,
        (Some((_, url, _)), false) => say(&format!(
            "kestrel is running, gateway on http://{addr}/. Open the UI at {url} . Ctrl-C stops it."
        ))?,
        (None, false) => say(&format!(
            "kestrel is running, gateway on http://{addr}/. Ctrl-C stops it."
        ))?,
    }
    if let Some((_, url, true)) = &shown {
        open_browser(url);
    }
    supervise(engine.run_forever(shutdown.clone()), server, shutdown).await
}

/// Runs the engine until it stops, and stops it early if the gateway dies. Either one stopping
/// cancels `shutdown` so the other stops too. A gateway that ends before shutdown is an error.
async fn supervise<E: std::fmt::Display>(
    engine: impl Future<Output = Result<(), E>>,
    mut server: tokio::task::JoinHandle<std::io::Result<()>>,
    shutdown: CancellationToken,
) -> Result<(), Failure> {
    let gateway = |r: Result<std::io::Result<()>, tokio::task::JoinError>, expected: bool| match r {
        Ok(Ok(())) if expected => Ok(()),
        Ok(Ok(())) => Err(Failure::runtime("gateway stopped unexpectedly")),
        Ok(Err(e)) => Err(Failure::runtime(format!("gateway: {e}"))),
        Err(e) => Err(Failure::runtime(format!("gateway: {e}"))),
    };
    tokio::pin!(engine);
    tokio::select! {
        result = &mut engine => {
            shutdown.cancel();
            let served = gateway(server.await, true);
            result.map_err(|e| Failure::runtime(e.to_string()))?;
            served
        }
        result = &mut server => {
            // Ctrl-C cancels `shutdown` first, so the gateway can finish before the engine does.
            // That is a clean shutdown: only a stop nobody asked for is an error.
            let asked_to_stop = shutdown.is_cancelled();
            shutdown.cancel();
            let served = gateway(result, asked_to_stop);
            if asked_to_stop {
                engine.await.map_err(|e| Failure::runtime(e.to_string()))?;
            }
            served
        }
    }
}

/// Resolves on the first SIGINT. The handler is installed before this returns.
#[cfg(unix)]
fn interrupt() -> Result<impl Future<Output = ()>, Failure> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sig = signal(SignalKind::interrupt())
        .map_err(|e| Failure::runtime(format!("listening for Ctrl-C: {e}")))?;
    Ok(async move {
        sig.recv().await;
    })
}

#[cfg(not(unix))]
fn interrupt() -> Result<impl Future<Output = ()>, Failure> {
    Ok(async {
        let _ = tokio::signal::ctrl_c().await;
    })
}

/// The gateway's view of the store: run tokens, budgets and metering.
struct StoreDeps(Arc<Store>);

#[async_trait]
impl RunResolver for StoreDeps {
    async fn resolve(&self, token: &str) -> Option<RunId> {
        self.0.run_by_token(token).ok().flatten()
    }
}

#[async_trait]
impl BudgetGuard for StoreDeps {
    /// A run may call a model while it has spent less than its current budget.
    async fn allow(&self, run: RunId) -> bool {
        match (self.0.spent(run), self.0.run_spec(run)) {
            (Ok(spent), Ok(Some(spec))) => spent < spec.budget.limit,
            _ => false,
        }
    }
}

#[async_trait]
impl MeterSink for StoreDeps {
    async fn record(&self, rec: ModelCallRecord) {
        // Spend first: it must count even if the event cannot be written.
        if let Err(e) = self.0.record_model_call(&rec) {
            eprintln!(
                "kestrel: recording a model call for run {}: {e}",
                rec.run_id
            );
        }
        let run = rec.run_id;
        if let Err(e) = self.0.append(run, EventKind::ModelCall(rec)) {
            eprintln!("kestrel: adding a model_call event to run {run}: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kestrel_types::AgentSpec;

    // #37 scenario 2: the API server binds 127.0.0.1 by default; another address starts it and
    // warns.
    #[test]
    fn api_listen_defaults_to_loopback_without_a_warning() {
        let (addr, warning) = api_listen(None).unwrap();
        assert_eq!(addr, "127.0.0.1:7171".parse::<SocketAddr>().unwrap());
        assert_eq!(warning, None);
        let (addr, warning) = api_listen(Some("127.0.0.1:9000")).unwrap();
        assert_eq!(addr.port(), 9000);
        assert_eq!(warning, None);
    }

    #[test]
    fn api_listen_off_loopback_warns() {
        let (addr, warning) = api_listen(Some("0.0.0.0:7171")).unwrap();
        assert!(addr.ip().is_unspecified());
        let warning = warning.unwrap();
        assert!(
            warning.contains("0.0.0.0:7171") && warning.contains("not loopback"),
            "{warning}"
        );
    }

    #[test]
    fn api_listen_rejects_a_bad_address() {
        let err = api_listen(Some("localhost")).unwrap_err();
        assert!(err.message.contains("--listen"), "{}", err.message);
    }

    #[test]
    fn the_ui_url_carries_the_token_and_names_a_reachable_host() {
        let loopback: SocketAddr = "127.0.0.1:7171".parse().unwrap();
        assert_eq!(ui_url(loopback, "abc"), "http://127.0.0.1:7171/?token=abc");
        let any: SocketAddr = "0.0.0.0:7171".parse().unwrap();
        assert_eq!(ui_url(any, "abc"), "http://127.0.0.1:7171/?token=abc");
        let any6: SocketAddr = "[::]:7171".parse().unwrap();
        assert_eq!(ui_url(any6, "abc"), "http://[::1]:7171/?token=abc");
        let lan: SocketAddr = "192.168.1.20:7171".parse().unwrap();
        assert_eq!(ui_url(lan, "abc"), "http://192.168.1.20:7171/?token=abc");
    }

    #[test]
    fn the_browser_is_opened_with_the_platform_command() {
        let url = "http://127.0.0.1:7171/?token=abc";
        assert_eq!(
            browser_command("macos", url),
            Some(("open".into(), vec![url.into()]))
        );
        assert_eq!(
            browser_command("linux", url),
            Some(("xdg-open".into(), vec![url.into()]))
        );
        assert_eq!(
            browser_command("windows", url),
            Some(("explorer".into(), vec![url.into()]))
        );
        assert_eq!(browser_command("plan9", url), None);
    }

    #[test]
    fn the_token_file_lives_next_to_the_database() {
        let db = Path::new("/data/kestrel/kestrel.db");
        assert_eq!(token_path(db), Path::new("/data/kestrel/server.token"));
    }

    const YAML: &str = "
name: t
runtime: claude-code
role: tester
model_policy: { default: tier:mid }
sandbox: { tier: auto, cpu: 1, memory: 1Gi, workspace: empty-dir }
budget: { usd: 0.000010, wall_clock: 10m }
";

    fn call(run_id: RunId, cost: i64) -> ModelCallRecord {
        ModelCallRecord {
            run_id,
            provider: "anthropic".into(),
            model: "m".into(),
            tokens_in: 1,
            tokens_out: 1,
            tokens_cache_read: 0,
            tokens_cache_write: 0,
            cost: Some(MicroUsd(cost)),
            latency_ms: 1,
            status: 200,
        }
    }

    #[tokio::test]
    async fn store_deps_resolve_tokens_meter_calls_and_guard_the_budget() {
        let store = Arc::new(Store::open_in_memory().expect("store"));
        let spec = AgentSpec::from_yaml(YAML).expect("spec");
        let id = store.create_run(&spec, "x").expect("create");
        let token = store.run_token(id).expect("token").expect("has token");
        let deps = StoreDeps(store.clone());

        assert_eq!(deps.resolve(&token).await, Some(id));
        assert_eq!(deps.resolve("nope").await, None);

        assert!(deps.allow(id).await);
        deps.record(call(id, 10)).await;
        assert_eq!(store.spent(id).expect("spent"), MicroUsd(10));
        assert!(
            !deps.allow(id).await,
            "spent reached the 10 micro-USD budget"
        );
        store.set_budget(id, MicroUsd(11)).expect("raise");
        assert!(deps.allow(id).await, "a raised budget allows calls again");
        assert!(!deps.allow(RunId::new()).await, "unknown runs are refused");
    }

    fn write(config: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, config).expect("write");
        (dir, path)
    }

    fn err(config: &str) -> String {
        let (_dir, path) = write(config);
        let e = load_config(&path).expect_err("config is rejected");
        assert_eq!(e.code, crate::EXIT_USAGE, "{}", e.message);
        e.message
    }

    const ANTHROPIC: &str = "[providers.anthropic]\napi_key = \"k\"\n";

    #[test]
    fn defaults_fill_in_everything_but_the_key() {
        let (_dir, path) = write(ANTHROPIC);
        let c = load_config(&path).expect("config");
        assert_eq!(c.listen, DEFAULT_LISTEN);
        assert_eq!(c.limits, Limits::default());
        assert_eq!(c.models, default_models());
        assert_eq!(
            c.gateway.anthropic.base_url.as_str(),
            "https://api.anthropic.com/"
        );
        assert_eq!(
            c.gateway.openai.base_url.as_str(),
            "https://api.openai.com/"
        );
        assert_eq!(c.gateway.openai.api_key, "");
        assert!(c.gateway.prices.is_empty());
    }

    #[test]
    fn models_and_prices_are_read_exactly() {
        let (_dir, path) = write(&format!(
            "{ANTHROPIC}[models]\ncheap = \"c\"\nfrontier = \"f\"\n\
             [prices.c]\ninput_per_mtok = \"0.8\"\noutput_per_mtok = \"4\"\n\
             cache_read_per_mtok = \"0.08\"\ncache_write_per_mtok = \"1\"\n"
        ));
        let c = load_config(&path).expect("config");
        assert_eq!(c.models[&Tier::Cheap], "c");
        assert_eq!(c.models[&Tier::Mid], "claude-sonnet-5-5");
        assert_eq!(c.models[&Tier::Frontier], "f");
        assert_eq!(c.gateway.prices["c"].input_per_mtok, MicroUsd(800_000));
        assert_eq!(c.gateway.prices["c"].cache_read_per_mtok, MicroUsd(80_000));
    }

    #[test]
    fn openai_alone_is_enough() {
        let (_dir, path) = write("[providers.openai]\napi_key = \"k\"\n");
        let c = load_config(&path).expect("config");
        assert_eq!(c.gateway.openai.api_key, "k");
        assert_eq!(c.gateway.anthropic.api_key, "");
    }

    #[test]
    fn bad_values_name_their_key() {
        let tier = err(&format!("{ANTHROPIC}[models]\nhuge = \"x\"\n"));
        assert!(
            tier.contains("models: unknown tier name") && !tier.contains("huge"),
            "{tier}"
        );
        assert!(
            err(&format!("{ANTHROPIC}[engine]\nmax_concurrent_runs = 0\n"))
                .contains("engine.max_concurrent_runs")
        );
        assert!(
            err("[providers.openai]\napi_key = \"k\"\nbase_url = \"not a url\"\n")
                .contains("providers.openai.base_url")
        );
        let typo = err(&format!("{ANTHROPIC}[gatway]\n"));
        assert!(
            typo.contains("`***`") && typo.contains("`gateway`"),
            "{typo}"
        );
    }

    #[test]
    fn config_debug_hides_api_keys() {
        let (_dir, path) = write(
            "[providers.anthropic]\napi_key = \"sk-ant-hidden-1\"\n\
             [providers.openai]\napi_key = \"sk-oai-hidden-2\"\n",
        );
        let text = format!("{:?}", load_config(&path).expect("config"));
        assert!(!text.contains("hidden"), "{text}");
    }

    #[test]
    fn toml_errors_name_the_line_but_never_the_value() {
        for (config, secret) in [
            (
                "[providers.anthropic]\napi_key = sk-ant-SECRET1\n",
                "SECRET1",
            ),
            (
                "[providers.anthropic]\napi_kye = \"sk-ant-SECRET2\"\n",
                "SECRET2",
            ),
            (
                "[providers.anthropic]\napi_key = 4242424242\n",
                "4242424242",
            ),
            (
                "[providers.anthropic]\napi_key = 'sk-SECRET3'\nx = [\n",
                "SECRET3",
            ),
        ] {
            let msg = err(config);
            assert!(!msg.contains(secret), "{msg}");
            assert!(msg.contains("config.toml:"), "{msg}");
        }
        assert!(err("[providers.anthropic]\napi_key = 4242424242\n").contains("config.toml:2:"));
    }

    fn server(result: std::io::Result<()>) -> tokio::task::JoinHandle<std::io::Result<()>> {
        tokio::spawn(async move { result })
    }

    #[tokio::test]
    async fn a_dead_gateway_stops_the_engine_with_an_error() {
        let shutdown = CancellationToken::new();
        let token = shutdown.clone();
        let engine = async move {
            token.cancelled().await;
            Ok::<(), String>(())
        };
        let failed = server(Err(std::io::Error::other("accept failed")));
        let e = supervise(engine, failed, shutdown.clone())
            .await
            .expect_err("gateway error");
        assert!(e.message.contains("accept failed"), "{}", e.message);
        assert!(shutdown.is_cancelled());

        let engine = std::future::pending::<Result<(), String>>();
        let e = supervise(engine, server(Ok(())), CancellationToken::new())
            .await
            .expect_err("early stop");
        assert!(e.message.contains("unexpectedly"), "{}", e.message);

        let panicked = tokio::spawn(async { panic!("gateway panic") });
        let engine = std::future::pending::<Result<(), String>>();
        let e = supervise(engine, panicked, CancellationToken::new())
            .await
            .expect_err("panic");
        assert!(e.message.starts_with("gateway:"), "{}", e.message);
    }

    #[tokio::test]
    async fn the_engine_stopping_stops_the_gateway() {
        let shutdown = CancellationToken::new();
        let token = shutdown.clone();
        let gateway = tokio::spawn(async move {
            token.cancelled().await;
            Ok(())
        });
        supervise(async { Ok::<(), String>(()) }, gateway, shutdown.clone())
            .await
            .expect("clean stop");
        assert!(shutdown.is_cancelled());

        let e = supervise(
            async { Err::<(), String>("engine broke".into()) },
            server(Ok(())),
            CancellationToken::new(),
        )
        .await
        .expect_err("engine error");
        assert_eq!(e.message, "engine broke");
    }

    /// Ctrl-C cancels the shutdown token. The gateway stops at once, and the engine needs a few
    /// more steps to wind down. That order must be a clean shutdown, not a crashed gateway, and the
    /// engine must still be driven to the end.
    #[tokio::test]
    async fn an_interrupt_that_stops_the_gateway_first_is_a_clean_shutdown() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let shutdown = CancellationToken::new();
        let gateway_token = shutdown.clone();
        let gateway = tokio::spawn(async move {
            gateway_token.cancelled().await;
            Ok(())
        });
        let engine_token = shutdown.clone();
        let wound_down = Arc::new(AtomicBool::new(false));
        let flag = wound_down.clone();
        let engine = async move {
            engine_token.cancelled().await;
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            flag.store(true, Ordering::SeqCst);
            Ok::<(), String>(())
        };
        let interrupt = shutdown.clone();
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            interrupt.cancel();
        });

        supervise(engine, gateway, shutdown)
            .await
            .expect("an interrupt is a clean shutdown");
        assert!(
            wound_down.load(Ordering::SeqCst),
            "the engine must finish winding down"
        );
    }

    #[cfg(unix)]
    #[test]
    fn only_private_config_files_pass_without_a_warning() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, path) = write(ANTHROPIC);
        let set = |mode| {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod")
        };
        set(0o600);
        assert_eq!(permission_warning(&path), None);
        set(0o640);
        let w = permission_warning(&path).expect("warning");
        assert!(w.contains("640") && w.contains("chmod 600"), "{w}");
        assert_eq!(permission_warning(&path.with_extension("gone")), None);
    }

    #[test]
    fn mask_quoted_keeps_only_config_names_and_punctuation() {
        assert_eq!(
            mask_quoted("unknown field `api_kye`, expected `base_url` or `api_key`"),
            "unknown field `***`, expected `base_url` or `api_key`"
        );
        assert_eq!(
            mask_quoted("invalid string\nexpected `\"`, `'`"),
            "invalid string\nexpected `\"`, `'`"
        );
        assert_eq!(
            mask_quoted("invalid type: string \"sk-x\", expected usize"),
            "invalid type: string \"***\", expected usize"
        );
        assert_eq!(mask_quoted("bad `sk-unclosed"), "bad `***");
        assert_eq!(mask_quoted("no quotes"), "no quotes");
    }

    fn url(text: &str) -> Url {
        Url::parse(text).expect("url")
    }

    #[test]
    fn the_gateway_url_prefers_advertise_then_bridge_then_the_bound_address() {
        let bound: SocketAddr = "127.0.0.1:7070".parse().expect("addr");
        let advertise = url("https://gw.example.com/");
        let bridge = url("http://172.18.0.2:7070/");
        let pick = |a: Option<&Url>, b: Option<&Url>| gateway_url_for(a, b, bound).expect("url");
        assert_eq!(pick(Some(&advertise), Some(&bridge)), advertise);
        assert_eq!(pick(Some(&advertise), None), advertise);
        assert_eq!(pick(None, Some(&bridge)), bridge);
        assert_eq!(pick(None, None).as_str(), "http://127.0.0.1:7070/");
    }

    #[test]
    fn advertise_url_is_read_and_checked_without_echo() {
        let (_dir, path) = write(&format!(
            "[gateway]\nadvertise_url = \"http://kestrel-gateway:7070/\"\n{ANTHROPIC}"
        ));
        let c = load_config(&path).expect("config");
        assert_eq!(
            c.advertise_url.expect("set").as_str(),
            "http://kestrel-gateway:7070/"
        );
        let (_dir, path) = write(ANTHROPIC);
        assert!(load_config(&path).expect("config").advertise_url.is_none());
        for bad in ["\"secret-host/x\"", "\"ftp://secret-host/\"", "\"\"", "42"] {
            let msg = err(&format!("[gateway]\nadvertise_url = {bad}\n{ANTHROPIC}"));
            assert!(msg.contains("gateway.advertise_url"), "{msg}");
            assert!(!msg.contains("secret-host"), "{msg}");
        }
    }

    struct Recorder {
        calls: std::sync::Mutex<Vec<Vec<String>>>,
        status: i32,
    }

    #[async_trait]
    impl CommandRunner for Recorder {
        async fn run(&self, argv: &[String]) -> std::io::Result<kestrel_sandbox::CommandOutput> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(argv.to_vec());
            Ok(kestrel_sandbox::CommandOutput {
                status: self.status,
                stdout: if argv.get(1).map(String::as_str) == Some("inspect") {
                    "172.18.0.2\n".into()
                } else {
                    String::new()
                },
                stderr: "boom".into(),
            })
        }
        async fn spawn(
            &self,
            _argv: &[String],
        ) -> std::io::Result<kestrel_sandbox::SpawnedProcess> {
            Err(std::io::Error::other("no spawn"))
        }
    }

    fn recorder(status: i32) -> Arc<Recorder> {
        Arc::new(Recorder {
            calls: std::sync::Mutex::new(Vec::new()),
            status,
        })
    }

    fn call_count(r: &Recorder) -> usize {
        r.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    #[tokio::test]
    async fn the_bridge_starts_only_for_docker_without_an_advertise_url() {
        let advertise = url("http://gw.example.com/");
        for (docker, advertise) in [
            (false, None),
            (true, Some(&advertise)),
            (false, Some(&advertise)),
        ] {
            let r = recorder(0);
            let bridge = start_bridge(
                r.clone(),
                DockerHost::Native,
                docker,
                advertise,
                7070,
                Path::new("/tmp/k.db"),
            )
            .await;
            assert!(bridge.expect("ok").is_none());
            assert_eq!(call_count(&r), 0);
        }
        let r = recorder(0);
        let bridge = start_bridge(
            r.clone(),
            DockerHost::Native,
            true,
            None,
            7070,
            Path::new("/tmp/k.db"),
        )
        .await
        .expect("started")
        .expect("bridge");
        assert_eq!(bridge.url().as_str(), "http://172.18.0.2:7070/");
        assert_eq!(call_count(&r), 5);
        bridge.stop().await;
        assert_eq!(call_count(&r), 6);
    }

    #[tokio::test]
    async fn a_failing_bridge_is_a_runtime_error() {
        let e = start_bridge(
            recorder(1),
            DockerHost::Native,
            true,
            None,
            7070,
            Path::new("/tmp/k.db"),
        )
        .await
        .expect_err("fails");
        assert!(e.message.contains("boom"), "{}", e.message);
    }

    #[test]
    fn a_loopback_gateway_with_a_bridge_is_a_config_error() {
        let loopback: SocketAddr = "127.0.0.1:7070".parse().expect("addr");
        let v6_loopback: SocketAddr = "[::1]:7070".parse().expect("addr");
        for addr in [loopback, v6_loopback] {
            let e = check_bridge_listen(true, addr).expect_err("loopback with a bridge");
            assert_eq!(e.code, crate::EXIT_USAGE);
            for needle in ["gateway.listen", "docker bridge", "gateway.advertise_url"] {
                assert!(e.message.contains(needle), "{}", e.message);
            }
            assert!(
                !e.message.contains("0.0.0.0:7070"),
                "no longer advises 0.0.0.0: {}",
                e.message
            );
            // No bridge (advertise_url set, or a driver that needs none): loopback is fine.
            assert_eq!(check_bridge_listen(false, addr).expect("ok"), None);
        }
    }

    #[test]
    fn an_all_interfaces_gateway_with_a_bridge_is_a_warning_about_the_lan() {
        for text in ["0.0.0.0:7070", "[::]:7070"] {
            let addr: SocketAddr = text.parse().expect("addr");
            let warning = check_bridge_listen(true, addr)
                .expect("a warning is not an error")
                .expect("a warning");
            assert!(warning.contains(text), "{warning}");
            assert!(warning.contains("LAN"), "{warning}");
            assert!(warning.contains("gateway.listen"), "{warning}");
            // Without a bridge the existing generic warning in `run` is enough.
            assert_eq!(check_bridge_listen(false, addr).expect("ok"), None);
        }
    }

    #[test]
    fn the_docker_bridge_ip_or_another_specific_address_is_silent() {
        for text in ["172.17.0.1:7070", "192.168.1.5:9000", "10.0.0.2:7070"] {
            let addr: SocketAddr = text.parse().expect("addr");
            assert_eq!(check_bridge_listen(true, addr).expect("ok"), None, "{text}");
            assert_eq!(
                check_bridge_listen(false, addr).expect("ok"),
                None,
                "{text}"
            );
        }
    }

    fn ip(text: &str) -> Ipv4Addr {
        text.parse().expect("ip")
    }

    #[test]
    fn the_bind_decision_table() {
        let bridge = Some(ip("172.17.0.1"));
        // An explicit listen is respected as written, whatever else is set.
        assert_eq!(
            gateway_bind(Some("0.0.0.0:9000"), Some(1234), bridge),
            "0.0.0.0:9000"
        );
        assert_eq!(
            gateway_bind(Some("127.0.0.1:0"), None, bridge),
            "127.0.0.1:0"
        );
        assert_eq!(gateway_bind(Some("10.1.2.3:80"), None, None), "10.1.2.3:80");
        // Not explicit, bridge IP known: bind the docker bridge, port 7070 or gateway.port.
        assert_eq!(gateway_bind(None, None, bridge), "172.17.0.1:7070");
        assert_eq!(gateway_bind(None, Some(9000), bridge), "172.17.0.1:9000");
        // Not explicit, no bridge: the loopback default, with gateway.port if set.
        assert_eq!(gateway_bind(None, None, None), DEFAULT_LISTEN);
        assert_eq!(gateway_bind(None, Some(9000), None), "127.0.0.1:9000");
    }

    #[test]
    fn the_bridge_gateway_answer_is_one_ipv4_address() {
        assert_eq!(
            parse_bridge_ip("172.17.0.1\n").expect("ip"),
            ip("172.17.0.1")
        );
        assert_eq!(
            parse_bridge_ip("  192.168.64.1 \r\n").expect("ip"),
            ip("192.168.64.1")
        );
        for bad in [
            "",
            "\n",
            "garbage",
            "<no value>",
            "::1",
            "999.1.1.1",
            "172.17.0.1 172.18.0.1",
            "172.17.0.1/16",
            "172.17.0.1\n172.18.0.1\n",
        ] {
            let e = parse_bridge_ip(bad).expect_err(bad);
            assert_eq!(e.code, crate::EXIT_RUNTIME, "{bad:?}");
            assert!(
                e.message.contains("docker bridge"),
                "{bad:?}: {}",
                e.message
            );
        }
    }

    /// A runner that answers every command the same way and records the argv.
    struct Scripted {
        calls: std::sync::Mutex<Vec<Vec<String>>>,
        answer: std::io::Result<kestrel_sandbox::CommandOutput>,
    }

    impl Scripted {
        fn new(status: i32, stdout: &str, stderr: &str) -> Scripted {
            Scripted {
                calls: std::sync::Mutex::new(Vec::new()),
                answer: Ok(kestrel_sandbox::CommandOutput {
                    status,
                    stdout: stdout.into(),
                    stderr: stderr.into(),
                }),
            }
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    #[async_trait]
    impl CommandRunner for Scripted {
        async fn run(&self, argv: &[String]) -> std::io::Result<kestrel_sandbox::CommandOutput> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(argv.to_vec());
            match &self.answer {
                Ok(out) => Ok(kestrel_sandbox::CommandOutput {
                    status: out.status,
                    stdout: out.stdout.clone(),
                    stderr: out.stderr.clone(),
                }),
                Err(e) => Err(std::io::Error::new(e.kind(), e.to_string())),
            }
        }
        async fn spawn(
            &self,
            _argv: &[String],
        ) -> std::io::Result<kestrel_sandbox::SpawnedProcess> {
            Err(std::io::Error::other("no spawn"))
        }
    }

    fn inspect_argv() -> Vec<String> {
        [
            "docker",
            "network",
            "inspect",
            "bridge",
            "--format",
            "{{(index .IPAM.Config 0).Gateway}}",
        ]
        .iter()
        .map(|p| (*p).to_string())
        .collect()
    }

    #[tokio::test]
    async fn discovery_runs_the_pinned_docker_command_and_parses_the_answer() {
        let r = Scripted::new(0, "172.17.0.1\n", "");
        assert_eq!(discover_bridge_ip(&r).await.expect("ip"), ip("172.17.0.1"));
        assert_eq!(r.calls(), vec![inspect_argv()]);
    }

    #[tokio::test]
    async fn discovery_failures_are_runtime_errors_that_say_what_to_do() {
        let cases = [
            Scripted::new(0, "", ""),
            Scripted::new(0, "not an ip", ""),
            Scripted::new(1, "", "Cannot connect to the Docker daemon"),
            Scripted {
                calls: std::sync::Mutex::new(Vec::new()),
                answer: Err(std::io::Error::other("docker: not found")),
            },
        ];
        for r in cases {
            let e = discover_bridge_ip(&r).await.expect_err("fails");
            assert_eq!(e.code, crate::EXIT_RUNTIME);
            for needle in ["docker network inspect bridge", "gateway.listen"] {
                assert!(e.message.contains(needle), "{}", e.message);
            }
        }
        let daemon = Scripted::new(1, "", "Cannot connect to the Docker daemon");
        let e = discover_bridge_ip(&daemon).await.expect_err("fails");
        assert!(e.message.contains("Cannot connect"), "{}", e.message);
    }

    #[tokio::test]
    async fn docker_is_asked_only_for_a_bridge_without_an_explicit_listen() {
        // Explicit listen: respected, docker never asked.
        let r = Scripted::new(0, "172.17.0.1\n", "");
        let bind = resolve_listen(&r, Some("0.0.0.0:7070"), Some(1), true).await;
        assert_eq!(bind.expect("bind"), "0.0.0.0:7070");
        // No bridge needed: loopback default, docker never asked.
        let bind = resolve_listen(&r, None, None, false).await;
        assert_eq!(bind.expect("bind"), DEFAULT_LISTEN);
        assert!(r.calls().is_empty());
        // Bridge and no explicit listen: the docker bridge IP, one call.
        let bind = resolve_listen(&r, None, None, true).await;
        assert_eq!(bind.expect("bind"), "172.17.0.1:7070");
        let bind = resolve_listen(&r, None, Some(9000), true).await;
        assert_eq!(bind.expect("bind"), "172.17.0.1:9000");
        assert_eq!(r.calls(), vec![inspect_argv(), inspect_argv()]);
    }

    #[tokio::test]
    async fn a_failing_discovery_fails_the_bind_unless_listen_is_explicit() {
        let r = Scripted::new(1, "", "daemon down");
        assert!(resolve_listen(&r, None, None, true).await.is_err());
        let bind = resolve_listen(&r, Some("127.0.0.1:0"), None, true).await;
        assert_eq!(
            bind.expect("explicit listen needs no docker"),
            "127.0.0.1:0"
        );
    }

    #[test]
    fn gateway_port_and_listen_are_read_from_the_config() {
        let (_dir, path) = write(&format!("[gateway]\nport = 9000\n{ANTHROPIC}"));
        let c = load_config(&path).expect("config");
        assert_eq!(c.port, Some(9000));
        assert!(!c.listen_explicit);
        let (_dir, path) = write(&format!("[gateway]\nlisten = \"127.0.0.1:1\"\n{ANTHROPIC}"));
        let c = load_config(&path).expect("config");
        assert!(c.listen_explicit);
        assert_eq!(c.listen, "127.0.0.1:1");
        assert_eq!(c.port, None);
        let (_dir, path) = write(ANTHROPIC);
        let c = load_config(&path).expect("config");
        assert!(!c.listen_explicit);
        assert_eq!(c.port, None);
    }

    #[test]
    fn a_bad_gateway_port_is_a_usage_error_naming_the_key() {
        for bad in ["0", "70000", "-1", "\"7070\"", "true"] {
            let msg = err(&format!("[gateway]\nport = {bad}\n{ANTHROPIC}"));
            assert!(msg.contains("port"), "{bad}: {msg}");
        }
    }

    #[test]
    fn the_lock_file_sits_next_to_the_database() {
        assert_eq!(
            serve_lock_path(Path::new("/data/kestrel.db")),
            PathBuf::from("/data/kestrel.db.serve.lock")
        );
        assert_eq!(
            serve_lock_path(Path::new("other.sqlite")),
            PathBuf::from("other.sqlite.serve.lock")
        );
    }

    #[test]
    fn a_second_serve_on_the_same_database_is_refused_naming_the_lock_file() {
        let dir = tempfile::tempdir().expect("dir");
        let db = dir.path().join("kestrel.db");
        let first = acquire_serve_lock(&db).expect("first lock");
        let lock = serve_lock_path(&db);
        assert!(lock.exists());
        let e = acquire_serve_lock(&db)
            .err()
            .expect("second lock is refused");
        assert_eq!(e.code, crate::EXIT_RUNTIME);
        assert!(
            e.message.contains(&lock.display().to_string()),
            "{}",
            e.message
        );
        drop(first);
        acquire_serve_lock(&db).expect("the lock is free again after the first holder drops");
    }

    #[test]
    fn a_lock_file_that_cannot_be_created_is_a_runtime_error() {
        let dir = tempfile::tempdir().expect("dir");
        let e = acquire_serve_lock(&dir.path().join("missing").join("k.db"))
            .err()
            .expect("no directory");
        assert_eq!(e.code, crate::EXIT_RUNTIME);
        assert!(e.message.contains("k.db.serve.lock"), "{}", e.message);
    }

    #[test]
    fn two_databases_lock_independently() {
        let dir = tempfile::tempdir().expect("dir");
        let a = acquire_serve_lock(&dir.path().join("a.db")).expect("a");
        let b = acquire_serve_lock(&dir.path().join("b.db")).expect("b");
        drop((a, b));
    }

    fn run_in(store: &Store) -> RunId {
        let spec = AgentSpec::from_yaml(YAML).expect("spec");
        store.create_run(&spec, "x").expect("create")
    }

    fn model_calls(store: &Store, run: RunId) -> Vec<(u64, ModelCallRecord)> {
        store
            .events_since(run, 0)
            .expect("events")
            .into_iter()
            .filter_map(|e| match e.kind {
                EventKind::ModelCall(rec) => Some((e.seq, rec)),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn metering_a_call_adds_spend_and_one_model_call_event() {
        let store = Arc::new(Store::open_in_memory().expect("store"));
        let run = run_in(&store);
        let deps = StoreDeps(store.clone());
        let first = call(run, 3);
        let second = call(run, 4);
        deps.record(first.clone()).await;
        deps.record(second.clone()).await;
        assert_eq!(store.spent(run).expect("spent"), MicroUsd(7));
        let seen = model_calls(&store, run);
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].1, first);
        assert_eq!(seen[1].1, second);
        assert_eq!(seen[1].0, seen[0].0 + 1, "consecutive seqs");
    }

    #[tokio::test]
    async fn an_unpriced_call_is_an_event_but_costs_nothing() {
        let store = Arc::new(Store::open_in_memory().expect("store"));
        let run = run_in(&store);
        let mut rec = call(run, 0);
        rec.cost = None;
        StoreDeps(store.clone()).record(rec.clone()).await;
        assert_eq!(store.spent(run).expect("spent"), MicroUsd(0));
        let seen = model_calls(&store, run);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].1, rec);
    }

    #[tokio::test]
    async fn a_call_for_an_unknown_run_appends_nothing_and_does_not_panic() {
        let store = Arc::new(Store::open_in_memory().expect("store"));
        let known = run_in(&store);
        StoreDeps(store.clone()).record(call(RunId::new(), 5)).await;
        assert!(model_calls(&store, known).is_empty());
        assert_eq!(store.spent(known).expect("spent"), MicroUsd(0));
    }

    #[tokio::test]
    async fn metering_keeps_event_seqs_gapless_next_to_another_writer() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("kestrel.db");
        let gateway_side = Arc::new(Store::open(&path).expect("store"));
        let engine_side = Store::open(&path).expect("store");
        let run = run_in(&gateway_side);
        let deps = StoreDeps(gateway_side.clone());
        for n in 0..5 {
            deps.record(call(run, 1)).await;
            engine_side
                .append(
                    run,
                    EventKind::AgentLog {
                        line: format!("log {n}"),
                    },
                )
                .expect("append");
        }
        let seqs: Vec<u64> = gateway_side
            .events_since(run, 0)
            .expect("events")
            .iter()
            .map(|e| e.seq)
            .collect();
        let expected: Vec<u64> = (seqs[0]..seqs[0] + 10).collect();
        assert_eq!(seqs, expected, "no gaps, no duplicates");
        assert_eq!(model_calls(&gateway_side, run).len(), 5);
    }

    // ---- gateway.docker_host (issue #53) ----

    const SECRET: &str = "secret-mode-9f3a";

    fn host_of(config: &str) -> DockerHostSetting {
        let (_dir, path) = write(config);
        load_config(&path).expect("config").docker_host
    }

    #[test]
    fn docker_host_defaults_to_auto_and_reads_the_three_values() {
        assert_eq!(host_of(ANTHROPIC), DockerHostSetting::Auto);
        for (text, want) in [
            ("auto", DockerHostSetting::Auto),
            ("native", DockerHostSetting::Native),
            ("vm", DockerHostSetting::Vm),
        ] {
            let config = format!("[gateway]\ndocker_host = \"{text}\"\n{ANTHROPIC}");
            assert_eq!(host_of(&config), want, "{text}");
        }
    }

    #[test]
    fn any_other_docker_host_is_a_usage_error_naming_the_key_and_hiding_the_value() {
        let bad = [
            format!("\"{SECRET}\""),
            "\"VM\"".to_string(),
            "\"Auto\"".to_string(),
            "\"\"".to_string(),
            "\" vm\"".to_string(),
            "42".to_string(),
            "true".to_string(),
            "[\"vm\"]".to_string(),
            format!("{{ x = \"{SECRET}\" }}"),
        ];
        for value in bad {
            let msg = err(&format!("[gateway]\ndocker_host = {value}\n{ANTHROPIC}"));
            assert!(msg.contains("gateway.docker_host"), "{value}: {msg}");
            assert!(!msg.contains(SECRET), "{value}: {msg}");
        }
    }

    #[test]
    fn docker_host_is_a_known_config_name_so_toml_errors_can_quote_it() {
        assert_eq!(
            mask_quoted("duplicate key `docker_host` in table `gateway`"),
            "duplicate key `docker_host` in table `gateway`"
        );
        let msg = err(&format!(
            "[gateway]\ndocker_host = \"vm\"\ndocker_host = \"vm\"\n{ANTHROPIC}"
        ));
        assert!(msg.contains("`docker_host`"), "{msg}");
    }

    fn io_err(kind: std::io::ErrorKind) -> std::io::Result<()> {
        Err(std::io::Error::from(kind))
    }

    #[test]
    fn explicit_native_and_vm_win_whatever_the_bind_said() {
        use std::io::ErrorKind::{AddrInUse, AddrNotAvailable, PermissionDenied};
        for (setting, want) in [
            (DockerHostSetting::Native, DockerHost::Native),
            (DockerHostSetting::Vm, DockerHost::Vm),
        ] {
            for bind in [
                Ok(()),
                io_err(AddrNotAvailable),
                io_err(PermissionDenied),
                io_err(AddrInUse),
            ] {
                assert_eq!(choose_docker_host(setting, &bind).expect("mode"), want);
            }
        }
    }

    #[test]
    fn auto_is_native_when_the_bridge_ip_binds_and_vm_when_it_is_not_local() {
        assert_eq!(
            choose_docker_host(DockerHostSetting::Auto, &Ok(())).expect("mode"),
            DockerHost::Native
        );
        assert_eq!(
            choose_docker_host(
                DockerHostSetting::Auto,
                &io_err(std::io::ErrorKind::AddrNotAvailable)
            )
            .expect("mode"),
            DockerHost::Vm
        );
    }

    #[test]
    fn auto_with_any_other_bind_error_is_a_runtime_error_naming_gateway_listen() {
        use std::io::ErrorKind::{AddrInUse, PermissionDenied};
        for kind in [PermissionDenied, AddrInUse, std::io::ErrorKind::Other] {
            let e = choose_docker_host(DockerHostSetting::Auto, &io_err(kind)).expect_err("fails");
            assert_eq!(e.code, crate::EXIT_RUNTIME, "{kind:?}");
            assert!(e.message.contains("gateway.listen"), "{}", e.message);
        }
    }

    const NATIVE_LINE: &str = "gateway: docker daemon on this host\n";
    const VM_LINE: &str = "gateway: docker daemon in a VM, binding loopback\n";

    /// 127.0.0.1 is always a local address; 203.0.113.5 (TEST-NET-3) never is.
    const LOCAL: &str = "127.0.0.1\n";
    const NOT_LOCAL: &str = "203.0.113.5\n";

    async fn gateway(
        r: &Scripted,
        setting: DockerHostSetting,
        listen: Option<&str>,
        port: Option<u16>,
        needs_bridge: bool,
    ) -> (Result<(DockerHost, String), Failure>, String) {
        let mut line = Vec::new();
        let got = resolve_gateway(r, setting, listen, port, needs_bridge, &mut line).await;
        (got, String::from_utf8(line).expect("utf8"))
    }

    #[tokio::test]
    async fn auto_with_a_local_bridge_ip_is_native_and_binds_that_ip() {
        let r = Scripted::new(0, LOCAL, "");
        let (got, line) = gateway(&r, DockerHostSetting::Auto, None, Some(9000), true).await;
        assert_eq!(
            got.expect("ok"),
            (DockerHost::Native, "127.0.0.1:9000".into())
        );
        assert_eq!(line, NATIVE_LINE);
        assert_eq!(r.calls(), vec![inspect_argv()]);
    }

    #[tokio::test]
    async fn auto_with_a_bridge_ip_that_is_not_local_is_vm_and_binds_loopback() {
        let r = Scripted::new(0, NOT_LOCAL, "");
        let (got, line) = gateway(&r, DockerHostSetting::Auto, None, None, true).await;
        assert_eq!(got.expect("ok"), (DockerHost::Vm, "127.0.0.1:7070".into()));
        assert_eq!(line, VM_LINE);
        let (got, _) = gateway(&r, DockerHostSetting::Auto, None, Some(9000), true).await;
        assert_eq!(got.expect("ok").1, "127.0.0.1:9000");
    }

    #[tokio::test]
    async fn an_explicit_listen_wins_in_every_mode_and_the_mode_is_still_chosen() {
        let r = Scripted::new(0, NOT_LOCAL, "");
        let (got, line) = gateway(
            &r,
            DockerHostSetting::Auto,
            Some("0.0.0.0:8123"),
            None,
            true,
        )
        .await;
        assert_eq!(got.expect("ok"), (DockerHost::Vm, "0.0.0.0:8123".into()));
        assert_eq!(line, VM_LINE);
        for (setting, host, text) in [
            (DockerHostSetting::Native, DockerHost::Native, NATIVE_LINE),
            (DockerHostSetting::Vm, DockerHost::Vm, VM_LINE),
        ] {
            let (got, line) = gateway(&r, setting, Some("10.1.2.3:80"), Some(1), true).await;
            assert_eq!(got.expect("ok"), (host, "10.1.2.3:80".into()));
            assert_eq!(line, text);
        }
    }

    #[tokio::test]
    async fn auto_with_an_explicit_listen_degrades_to_native_when_discovery_fails() {
        for r in [
            Scripted::new(1, "", "Cannot connect to the Docker daemon"),
            Scripted::new(0, "fd00::1\n", ""),
            Scripted::new(0, "not an address\n", ""),
        ] {
            let (got, line) = gateway(
                &r,
                DockerHostSetting::Auto,
                Some("10.1.2.3:8123"),
                None,
                true,
            )
            .await;
            assert_eq!(
                got.expect("ok"),
                (DockerHost::Native, "10.1.2.3:8123".into())
            );
            assert_eq!(line, NATIVE_LINE);
        }
    }

    #[tokio::test]
    async fn auto_without_an_explicit_listen_still_fails_on_a_non_ipv4_answer() {
        let r = Scripted::new(0, "fd00::1\n", "");
        let (got, line) = gateway(&r, DockerHostSetting::Auto, None, None, true).await;
        assert_eq!(got.expect_err("fails").code, crate::EXIT_RUNTIME);
        assert_eq!(line, "");
    }

    #[tokio::test]
    async fn explicit_native_binds_the_bridge_ip_as_today() {
        let r = Scripted::new(0, "172.17.0.1\n", "");
        let (got, line) = gateway(&r, DockerHostSetting::Native, None, None, true).await;
        assert_eq!(
            got.expect("ok"),
            (DockerHost::Native, "172.17.0.1:7070".into())
        );
        assert_eq!(line, NATIVE_LINE);
        assert_eq!(r.calls(), vec![inspect_argv()]);
    }

    #[tokio::test]
    async fn explicit_vm_never_asks_docker_and_binds_loopback() {
        let r = Scripted::new(1, "", "daemon down");
        let (got, line) = gateway(&r, DockerHostSetting::Vm, None, None, true).await;
        assert_eq!(got.expect("ok"), (DockerHost::Vm, "127.0.0.1:7070".into()));
        assert_eq!(line, VM_LINE);
        let (got, _) = gateway(&r, DockerHostSetting::Vm, None, Some(9000), true).await;
        assert_eq!(got.expect("ok").1, "127.0.0.1:9000");
        assert!(r.calls().is_empty());
    }

    #[tokio::test]
    async fn without_a_bridge_nothing_is_asked_or_printed() {
        let r = Scripted::new(0, NOT_LOCAL, "");
        for setting in [
            DockerHostSetting::Auto,
            DockerHostSetting::Native,
            DockerHostSetting::Vm,
        ] {
            let (got, line) = gateway(&r, setting, None, None, false).await;
            assert_eq!(got.expect("ok").1, DEFAULT_LISTEN, "{setting:?}");
            assert_eq!(line, "", "{setting:?}");
        }
        assert!(r.calls().is_empty());
    }

    #[tokio::test]
    async fn auto_cannot_decide_when_docker_does_not_answer() {
        let r = Scripted::new(1, "", "Cannot connect to the Docker daemon");
        let (got, line) = gateway(&r, DockerHostSetting::Auto, None, None, true).await;
        let e = got.expect_err("fails");
        assert_eq!(e.code, crate::EXIT_RUNTIME);
        assert!(
            e.message.contains("docker network inspect bridge"),
            "{}",
            e.message
        );
        assert_eq!(line, "", "no mode line when no mode was chosen");
    }

    fn sock(text: &str) -> SocketAddr {
        text.parse().expect("addr")
    }

    #[test]
    fn vm_accepts_loopback_with_a_bridge_silently() {
        for text in ["127.0.0.1:7070", "127.0.0.1:0", "[::1]:7070"] {
            assert_eq!(
                check_host_listen(DockerHost::Vm, true, sock(text)).expect("accepted"),
                None,
                "{text}"
            );
        }
    }

    #[test]
    fn vm_still_warns_about_the_lan_for_an_all_interfaces_bind() {
        for text in ["0.0.0.0:7070", "[::]:7070"] {
            let warning = check_host_listen(DockerHost::Vm, true, sock(text))
                .expect("a warning is not an error")
                .expect("a warning");
            assert!(
                warning.contains(text) && warning.contains("LAN"),
                "{warning}"
            );
        }
        for text in ["192.168.1.5:9000", "10.0.0.2:7070"] {
            assert_eq!(
                check_host_listen(DockerHost::Vm, true, sock(text)).expect("ok"),
                None
            );
        }
        assert_eq!(
            check_host_listen(DockerHost::Vm, false, sock("0.0.0.0:7070")).expect("ok"),
            None
        );
    }

    #[test]
    fn native_checks_are_exactly_the_existing_ones() {
        for text in [
            "127.0.0.1:7070",
            "[::1]:7070",
            "0.0.0.0:7070",
            "[::]:7070",
            "172.17.0.1:7070",
        ] {
            for bridge in [true, false] {
                let a = check_host_listen(DockerHost::Native, bridge, sock(text));
                let b = check_bridge_listen(bridge, sock(text));
                match (a, b) {
                    (Ok(a), Ok(b)) => assert_eq!(a, b, "{text} {bridge}"),
                    (Err(a), Err(b)) => assert_eq!(a.message, b.message, "{text} {bridge}"),
                    (a, b) => panic!("{text} {bridge}: {a:?} vs {b:?}"),
                }
            }
        }
        let e = check_host_listen(DockerHost::Native, true, sock("127.0.0.1:7070"))
            .expect_err("native loopback with a bridge");
        assert_eq!(e.code, crate::EXIT_USAGE);
    }

    fn run_call(r: &Recorder) -> Vec<String> {
        r.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|a| a.get(1).map(String::as_str) == Some("run"))
            .expect("a docker run call")
            .clone()
    }

    #[tokio::test]
    async fn the_bridge_run_has_add_host_for_native_and_none_for_vm() {
        for (host, has) in [(DockerHost::Native, true), (DockerHost::Vm, false)] {
            let r = recorder(0);
            start_bridge(r.clone(), host, true, None, 7070, Path::new("/tmp/k.db"))
                .await
                .expect("started")
                .expect("bridge");
            let argv = run_call(&r);
            assert_eq!(argv.iter().any(|a| a == "--add-host"), has, "{host:?}");
            assert_eq!(
                argv.iter()
                    .any(|a| a == "host.docker.internal:host-gateway"),
                has,
                "{host:?}"
            );
        }
    }

    // ---- gateway.subnet_pool (issue #34, scenario 9) ----
    //
    // The checked value's field name and type in `Config` are left to the implementation, so these
    // tests read it through `Config`'s `Debug` output, which must show the pool's address.

    fn config_text(config: &str) -> String {
        let (_dir, path) = write(config);
        format!("{:?}", load_config(&path).expect("config"))
    }

    #[test]
    fn subnet_pool_defaults_to_the_kestrel_pool() {
        assert_eq!(kestrel_sandbox::DEFAULT_SUBNET_POOL, "10.200.0.0/16");
        let text = config_text(ANTHROPIC);
        assert!(text.contains("10.200.0.0"), "{text}");
    }

    #[test]
    fn a_custom_subnet_pool_is_read() {
        for pool in ["10.123.0.0/20", "172.30.8.0/29", "192.168.64.0/18"] {
            let text = config_text(&format!("[gateway]\nsubnet_pool = \"{pool}\"\n{ANTHROPIC}"));
            let addr = pool.split('/').next().unwrap_or_default();
            assert!(text.contains(addr), "{pool}: {text}");
            assert!(!text.contains("10.200.0.0"), "{pool}: {text}");
        }
    }

    #[test]
    fn an_invalid_subnet_pool_is_a_usage_error_naming_the_key() {
        for bad in [
            "banana",
            "10.200.0.0",
            "10.200.0.1/16",
            "10.200.0.0/33",
            "",
            "fd00::/8",
        ] {
            let msg = err(&format!("[gateway]\nsubnet_pool = \"{bad}\"\n{ANTHROPIC}"));
            assert!(msg.contains("gateway.subnet_pool"), "{bad:?}: {msg}");
        }
    }

    #[test]
    fn a_subnet_pool_smaller_than_one_block_is_a_usage_error_saying_so() {
        for small in ["10.200.0.0/30", "10.200.0.0/32"] {
            let msg = err(&format!(
                "[gateway]\nsubnet_pool = \"{small}\"\n{ANTHROPIC}"
            ));
            assert!(msg.contains("gateway.subnet_pool"), "{small}: {msg}");
            assert!(msg.contains("/29"), "{small}: {msg}");
        }
    }

    #[test]
    fn a_subnet_pool_of_the_wrong_type_is_a_usage_error() {
        for bad in ["42", "true", "[\"10.200.0.0/16\"]", "{ x = 1 }"] {
            // `err` checks the exit code is the usage one.
            err(&format!("[gateway]\nsubnet_pool = {bad}\n{ANTHROPIC}"));
        }
    }

    #[test]
    fn subnet_pool_is_a_known_config_name_so_toml_errors_can_quote_it() {
        assert_eq!(
            mask_quoted("duplicate key `subnet_pool` in table `gateway`"),
            "duplicate key `subnet_pool` in table `gateway`"
        );
        let msg = err(&format!(
            "[gateway]\nsubnet_pool = \"10.200.0.0/16\"\nsubnet_pool = \"10.200.0.0/16\"\n{ANTHROPIC}"
        ));
        assert!(msg.contains("`subnet_pool`"), "{msg}");
    }

    #[test]
    fn a_subnet_pool_error_never_echoes_a_provider_key() {
        let msg = err(
            "[gateway]\nsubnet_pool = \"banana\"\n[providers.anthropic]\napi_key = \"sk-ant-SECRET4\"\n",
        );
        assert!(!msg.contains("SECRET4"), "{msg}");
    }

    // ---- the subnet plan at startup (issue #34, scenario 3) ----

    /// Plays docker with networks on `subnets` and a host whose `ip -4 route show` prints
    /// `routes`. Records every argv.
    struct Host {
        calls: std::sync::Mutex<Vec<Vec<String>>>,
        subnets: &'static str,
        routes: &'static str,
        docker_fails: bool,
        /// What `docker network ls -q` prints when filtered to this instance's run networks.
        ours: &'static str,
    }

    impl Host {
        fn new(subnets: &'static str, routes: &'static str) -> Host {
            Host {
                calls: std::sync::Mutex::new(Vec::new()),
                subnets,
                routes,
                docker_fails: false,
                ours: "",
            }
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        fn asked_for_routes(&self) -> bool {
            self.calls()
                .iter()
                .any(|a| a.first().map(String::as_str) == Some("ip"))
        }
    }

    #[async_trait]
    impl CommandRunner for Host {
        async fn run(&self, argv: &[String]) -> std::io::Result<kestrel_sandbox::CommandOutput> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(argv.to_vec());
            let words: Vec<&str> = argv.iter().map(String::as_str).collect();
            let stdout = match words.as_slice() {
                ["ip", ..] => self.routes,
                ["docker", "network", "ls", "-q", "--filter", ..] => self.ours,
                ["docker", "network", "ls", ..] => "1a2b3c4d5e6f\n",
                ["docker", "network", "inspect", ..] => self.subnets,
                _ => "",
            };
            let failed = self.docker_fails && words.first() == Some(&"docker");
            Ok(kestrel_sandbox::CommandOutput {
                status: i32::from(failed),
                stdout: stdout.into(),
                stderr: if failed {
                    "Cannot connect".into()
                } else {
                    String::new()
                },
            })
        }
        async fn spawn(
            &self,
            _argv: &[String],
        ) -> std::io::Result<kestrel_sandbox::SpawnedProcess> {
            Err(std::io::Error::other("no spawn"))
        }
    }

    fn cidr(text: &str) -> Ipv4Cidr {
        Ipv4Cidr::parse(text).expect("cidr")
    }

    async fn plan(
        pool: &str,
        host: &Host,
        mode: DockerHost,
    ) -> (Result<Arc<SubnetPool>, Failure>, String) {
        let mut err = Vec::new();
        let got = subnet_plan(cidr(pool), host, mode, None, &mut err).await;
        (got, String::from_utf8_lossy(&err).into_owned())
    }

    /// The plan of instance `k1` for a `/29` pool on a docker whose only network, on that block,
    /// prints `ours` when listed as k1's run networks.
    async fn plan_of_k1(ours: &'static str) -> (Result<Arc<SubnetPool>, Failure>, Host) {
        let mut host = Host::new("10.200.0.0/29\n", "");
        host.ours = ours;
        let mut err = Vec::new();
        let pool = cidr("10.200.0.0/29");
        let got = subnet_plan(pool, &host, DockerHost::Vm, Some("k1"), &mut err).await;
        (got, host)
    }

    #[tokio::test]
    async fn our_own_leftover_run_network_does_not_shrink_the_plan() {
        // `recover` removes it before any block is reserved, so its block stays in the plan.
        let (got, host) = plan_of_k1("1a2b3c4d5e6f\n").await;
        assert_eq!(got.expect("plan").available(), 1);
        let ours: Vec<String> = [
            "docker",
            "network",
            "ls",
            "-q",
            "--filter",
            "label=kestrel.run",
            "--filter",
            "label=kestrel.instance=k1",
        ]
        .map(String::from)
        .to_vec();
        assert!(host.calls().contains(&ours), "{:?}", host.calls());
        assert!(
            !host
                .calls()
                .iter()
                .any(|a| a.get(2).map(String::as_str) == Some("inspect")),
            "our leftover is not inspected: {:?}",
            host.calls()
        );
    }

    #[tokio::test]
    async fn a_network_of_another_instance_or_a_user_still_shrinks_the_plan() {
        let (got, _host) = plan_of_k1("").await;
        let e = got.expect_err("the only block is taken");
        assert!(e.message.contains("gateway.subnet_pool"), "{}", e.message);
    }

    #[tokio::test]
    async fn the_plan_skips_docker_networks_and_says_how_many_blocks_are_free_in_one_line() {
        let host = Host::new("10.200.0.0/29 fd00::/64\n", "");
        let (got, line) = plan("10.200.0.0/16", &host, DockerHost::Vm).await;
        let pool = got.expect("plan");
        assert_eq!(pool.pool(), cidr("10.200.0.0/16"));
        assert_eq!(pool.available(), 8191);
        assert_eq!(
            pool.reserve().map(|s| s.cidr().to_string()),
            Some("10.200.0.8/29".to_string())
        );
        assert_eq!(line.lines().count(), 1, "{line}");
        assert!(
            line.contains("10.200.0.0/16") && line.contains("8191"),
            "{line}"
        );
    }

    #[tokio::test]
    async fn host_routes_are_read_only_when_docker_runs_on_this_host() {
        let host = Host::new("", "10.200.0.0/17 dev wg0 scope link\n");
        let (got, _) = plan("10.200.0.0/16", &host, DockerHost::Native).await;
        assert!(host.asked_for_routes());
        assert_eq!(got.expect("plan").available(), 4096);

        let host = Host::new("", "10.200.0.0/17 dev wg0 scope link\n");
        let (got, _) = plan("10.200.0.0/16", &host, DockerHost::Vm).await;
        assert!(
            !host.asked_for_routes(),
            "a VM's routes are not this host's"
        );
        assert_eq!(got.expect("plan").available(), 8192);
    }

    #[tokio::test]
    async fn s3_a_route_over_the_whole_pool_stops_serve_naming_the_pool_and_the_setting() {
        let host = Host::new("", "10.0.0.0/8 dev tun0 scope link\n");
        let (got, line) = plan("10.200.0.0/16", &host, DockerHost::Native).await;
        let e = got.expect_err("no free block");
        assert_eq!(e.code, crate::EXIT_RUNTIME);
        assert!(e.message.contains("10.200.0.0/16"), "{}", e.message);
        assert!(e.message.contains("gateway.subnet_pool"), "{}", e.message);
        assert!(line.is_empty(), "{line}");

        let host = Host::new("10.200.0.0/16\n", "");
        let (got, _) = plan("10.200.0.0/16", &host, DockerHost::Vm).await;
        assert!(got.is_err(), "a docker network over the whole pool too");
    }

    #[tokio::test]
    async fn a_docker_that_cannot_list_networks_stops_serve() {
        let mut host = Host::new("", "");
        host.docker_fails = true;
        let (got, _) = plan("10.200.0.0/16", &host, DockerHost::Vm).await;
        let e = got.expect_err("docker failed");
        assert_eq!(e.code, crate::EXIT_RUNTIME);
        assert!(e.message.contains("Cannot connect"), "{}", e.message);
    }

    #[test]
    fn only_docker_without_a_bridge_warns_that_sandboxes_share_a_network() {
        let advertise = url("http://gw.example.com/");
        let warning = shared_network_warning(true, Some(&advertise)).expect("warning");
        assert!(warning.contains("reach each other"), "{warning}");
        assert!(warning.contains("gateway.advertise_url"), "{warning}");
        assert!(shared_network_warning(true, None).is_none(), "the bridge");
        assert!(shared_network_warning(false, Some(&advertise)).is_none());
        assert!(shared_network_warning(false, None).is_none());
    }

    #[tokio::test]
    async fn the_bridge_names_the_container_drivers_join_to_run_networks() {
        let db = Path::new("/tmp/k.db");
        let bridge = start_bridge(recorder(0), DockerHost::Native, true, None, 7070, db)
            .await
            .expect("started")
            .expect("bridge");
        assert_eq!(
            bridge.name(),
            GatewayBridge::container_name(&GatewayBridge::instance_id(db))
        );
    }
}
