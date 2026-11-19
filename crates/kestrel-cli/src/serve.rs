//! `kestrel serve`: the engine and the LLM gateway in one process, until SIGINT.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use kestrel_core::{AdapterFactory, Engine, Limits, ModelMap, Workspaces, default_models};
use kestrel_gateway::{
    BudgetGuard, GatewayConfig, GatewayDeps, MeterSink, ModelPrice, ProviderConfig, RunResolver,
};
use kestrel_sandbox::{
    BridgeSpec, CommandRunner, GatewayBridge, HostInfo, ProcessRunner, SandboxDriver, detect,
};
use kestrel_store::Store;
use kestrel_types::{MicroUsd, ModelCallRecord, RunId, Tier};
use serde::Deserialize;
use serde_json::json;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::commands::say;
use crate::{Failure, Output, ServeArgs, db_path, open, paths};

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
    /// Kept as a raw value so a wrong type is reported against the key, not as a TOML error.
    advertise_url: Option<toml::Value>,
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
    /// Where sandboxes reach the gateway, when the operator sets it.
    advertise_url: Option<Url>,
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
    Ok(Config {
        listen: raw.gateway.listen.unwrap_or_else(|| DEFAULT_LISTEN.into()),
        advertise_url,
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

/// Names that may appear in a config error. Anything else quoted in an error came from the
/// user and may be an API key, so it is masked.
const CONFIG_NAMES: &[&str] = &[
    "gateway",
    "listen",
    "advertise_url",
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

/// Starts the bridge when sandboxes need one: docker drivers and no `advertise_url`.
async fn start_bridge(
    runner: Arc<dyn CommandRunner>,
    docker: bool,
    advertise: Option<&Url>,
    port: u16,
) -> Result<Option<GatewayBridge>, Failure> {
    if !docker || advertise.is_some() {
        return Ok(None);
    }
    GatewayBridge::start(runner, port, BridgeSpec::default())
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

pub(crate) fn serve(db: Option<PathBuf>, args: ServeArgs, out: Output) -> Result<(), Failure> {
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
    rt.block_on(run(db, config, drivers, out))
}

async fn run(
    db: Option<PathBuf>,
    config: Config,
    drivers: Option<Vec<Box<dyn SandboxDriver>>>,
    out: Output,
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
        None => detect(&HostInfo::current().await, false)
            .map_err(|e| Failure::runtime(e.to_string()))?,
    };
    let listener = TcpListener::bind(&config.listen)
        .await
        .map_err(|e| Failure::runtime(format!("gateway.listen: cannot listen there: {e}")))?;
    let addr = listener
        .local_addr()
        .map_err(|e| Failure::runtime(e.to_string()))?;
    if !addr.ip().is_loopback() {
        eprintln!(
            "gateway listens on {addr}, which is not loopback: only run tokens protect it, bind to a private interface if you can"
        );
    }
    let bridge = start_bridge(
        Arc::new(ProcessRunner),
        uses_docker(&drivers),
        config.advertise_url.as_ref(),
        addr.port(),
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
            };
            serve_bound(db, config, drivers, bound, out).await
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
}

async fn serve_bound(
    db: Option<PathBuf>,
    config: Config,
    drivers: Vec<Box<dyn SandboxDriver>>,
    bound: Bound,
    out: Output,
) -> Result<(), Failure> {
    let Bound {
        listener,
        addr,
        gateway_url,
        shutdown,
    } = bound;
    let db = db_path(db)?;
    let store = Arc::new(open(Some(db.clone()))?);
    // Workspaces live next to the database, so a temp `--db` keeps everything in one place.
    let workspace_root = std::path::absolute(&db)
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
    .with_workspaces(Workspaces::new(workspace_root, Arc::new(ProcessRunner)));
    engine
        .recover()
        .await
        .map_err(|e| Failure::runtime(e.to_string()))?;

    let deps = Arc::new(StoreDeps(store));
    let app = kestrel_gateway::router(
        config.gateway,
        GatewayDeps {
            resolver: deps.clone(),
            budget: deps.clone(),
            meter: deps,
        },
    );
    let server =
        axum::serve(listener, app).with_graceful_shutdown(shutdown.clone().cancelled_owned());
    let server = tokio::spawn(async move { server.await });

    if out.json {
        say(&json!({ "gateway": addr.to_string() }).to_string())?;
    } else {
        say(&format!(
            "kestrel is running, gateway on http://{addr}/. Ctrl-C stops it."
        ))?;
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
        if let Err(e) = self.0.record_model_call(&rec) {
            eprintln!("error: recording a model call for run {}: {e}", rec.run_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kestrel_types::AgentSpec;

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
        let bridge = url("http://kestrel-gateway:7070/");
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
                stdout: String::new(),
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
            let bridge = start_bridge(r.clone(), docker, advertise, 7070).await;
            assert!(bridge.expect("ok").is_none());
            assert_eq!(call_count(&r), 0);
        }
        let r = recorder(0);
        let bridge = start_bridge(r.clone(), true, None, 7070)
            .await
            .expect("started")
            .expect("bridge");
        assert_eq!(bridge.url().as_str(), "http://kestrel-gateway:7070/");
        assert_eq!(call_count(&r), 4);
        bridge.stop().await;
        assert_eq!(call_count(&r), 5);
    }

    #[tokio::test]
    async fn a_failing_bridge_is_a_runtime_error() {
        let e = start_bridge(recorder(1), true, None, 7070)
            .await
            .expect_err("fails");
        assert!(e.message.contains("boom"), "{}", e.message);
    }
}
