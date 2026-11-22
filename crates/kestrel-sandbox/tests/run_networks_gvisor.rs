//! Issues #34 and #10 on a real host: per-run networks from Kestrel's own subnet pool, with a real
//! `GatewayBridge` joined to each, sandboxes under gVisor (`--runtime=runsc`), and a tiny HTTP
//! server owned by the test playing the gateway on the host.
//!
//! Scenarios 4 to 8 of #34, the #10 isolation check, and a 10-sandbox parallel start. Needs docker
//! with the `runsc` runtime, `alpine:3.20` and the bridge image (`DEFAULT_BRIDGE_IMAGE`). Run with
//! `cargo test -p kestrel-sandbox --features integration --test run_networks_gvisor`.
//!
//! Rules that keep this robust on a CI runner:
//!
//! * Sandboxes reach everything by IP. Docker's DNS does not answer inside gVisor.
//! * Every test takes `SERIAL`: each starts its own bridge, and scenario 8 starves the docker
//!   daemon's default address pools on purpose.
//! * Names and labels carry a per-test unique id. A `Cleanup` guard removes every container and
//!   network the test made (by name and by label) when it is dropped, even after a panic.
//! * Each test draws blocks from its own `10.231.<n>.0/24` range, after excluding what docker and
//!   the host routes already use.

#![cfg(feature = "integration")]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::{BTreeMap, HashSet};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use kestrel_sandbox::{
    BridgeSpec, GatewayBridge, GvisorDriver, INSTANCE_LABEL, LaunchSpec, NetworkPolicy,
    PreparedNetwork, ProcessRunner, RUN_LABEL, SandboxDriver, SandboxHandle, SubnetPool,
    docker_subnets, host_routes,
};
use kestrel_types::RunId;
use url::Url;

const IMAGE: &str = "alpine:3.20";
/// What the test gateway answers to every request.
const BODY: &str = "kestrel-gateway-ok";
/// Upper bound for one docker step or one sandbox run.
const STEP: Duration = Duration::from_secs(240);

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// `[A-Za-z0-9]` only, unique per call in this process and across runs.
fn unique() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("t{:x}x{:x}x{n}", std::process::id(), nanos & 0xffff_ffff)
}

async fn within<T>(what: &str, fut: impl Future<Output = T>) -> T {
    tokio::time::timeout(STEP, fut)
        .await
        .unwrap_or_else(|_| panic!("timed out: {what}"))
}

// docker ------------------------------------------------------------------------------------------

fn docker(args: &[&str]) -> std::process::Output {
    Command::new("docker")
        .args(args)
        .output()
        .expect("run docker")
}

/// Runs docker, asserts success, returns trimmed stdout.
fn docker_ok(args: &[&str]) -> String {
    let out = docker(args);
    assert!(
        out.status.success(),
        "docker {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn network_exists(name: &str) -> bool {
    docker(&["network", "inspect", name]).status.success()
}

/// The IPv4 address of container `id` on `network`, waiting until docker has one.
async fn address_on(id: &str, network: &str) -> String {
    let format = format!("{{{{(index .NetworkSettings.Networks \"{network}\").IPAddress}}}}");
    for _ in 0..60 {
        let ip = docker_ok(&["inspect", "--format", &format, id]);
        if ip.parse::<Ipv4Addr>().is_ok() {
            return ip;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    panic!("container {id} got no address on {network}");
}

/// Removes, when dropped, everything a test made: the names it registered and anything carrying
/// its instance label.
struct Cleanup {
    instance: String,
    containers: Mutex<Vec<String>>,
    networks: Mutex<Vec<String>>,
}

impl Cleanup {
    fn new(instance: &str) -> Cleanup {
        Cleanup {
            instance: instance.to_string(),
            containers: Mutex::default(),
            networks: Mutex::default(),
        }
    }

    fn container(&self, id: &str) {
        self.containers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(id.to_string());
    }

    fn network(&self, name: &str) {
        self.networks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(name.to_string());
    }

    fn labelled(&self, what: &[&str]) -> Vec<String> {
        let mut found = Vec::new();
        for label in [INSTANCE_LABEL, "kestrel.test"] {
            let filter = format!("label={label}={}", self.instance);
            let mut args = what.to_vec();
            args.extend(["--filter", filter.as_str()]);
            if let Ok(out) = Command::new("docker").args(&args).output() {
                found.extend(
                    String::from_utf8_lossy(&out.stdout)
                        .lines()
                        .map(str::trim)
                        .filter(|l| !l.is_empty())
                        .map(String::from),
                );
            }
        }
        found
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let mut containers = self
            .containers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        containers.extend(self.labelled(&["ps", "-aq"]));
        for c in containers.iter().rev() {
            let _ = Command::new("docker").args(["rm", "--force", c]).output();
        }
        let mut networks = self
            .networks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        networks.extend(self.labelled(&["network", "ls", "-q"]));
        for n in networks.iter().rev() {
            let _ = Command::new("docker").args(["network", "rm", n]).output();
        }
    }
}

// the gateway -------------------------------------------------------------------------------------

/// A tiny HTTP server on `0.0.0.0` answering [`BODY`] to everything. Returns its port.
fn start_gateway() -> u16 {
    let listener = TcpListener::bind(("0.0.0.0", 0)).expect("bind the test gateway");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            std::thread::spawn(move || {
                let _ = conn.set_read_timeout(Some(Duration::from_secs(10)));
                let mut buf = [0_u8; 4096];
                let _ = conn.read(&mut buf);
                let body = format!("{BODY}\n");
                let reply = format!(
                    "HTTP/1.0 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = conn.write_all(reply.as_bytes());
            });
        }
    });
    port
}

/// A test gateway that holds connections, as Kestrel holds a waiting `join`. A request for
/// `/hold` is never answered (the connection stays open) and counts in `held`; `/count` answers
/// that count; anything else answers [`BODY`]. Returns its port.
fn start_holding_gateway(held: Arc<AtomicUsize>) -> u16 {
    let listener = TcpListener::bind(("0.0.0.0", 0)).expect("bind the test gateway");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            let held = held.clone();
            std::thread::spawn(move || {
                let _ = conn.set_read_timeout(Some(Duration::from_secs(10)));
                let mut buf = [0_u8; 4096];
                let n = conn.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                if path == "/hold" {
                    held.fetch_add(1, Ordering::SeqCst);
                    // Held open, silently, well past the test.
                    std::thread::sleep(Duration::from_secs(300));
                    return;
                }
                let body = if path == "/count" {
                    held.load(Ordering::SeqCst).to_string()
                } else {
                    BODY.to_string()
                };
                let reply = format!(
                    "HTTP/1.0 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = conn.write_all(reply.as_bytes());
            });
        }
    });
    port
}

// the world of one test ---------------------------------------------------------------------------

struct World {
    driver: Arc<GvisorDriver>,
    pool: Arc<SubnetPool>,
    bridge: GatewayBridge,
    bridge_name: String,
    port: u16,
    instance: String,
    cleanup: Arc<Cleanup>,
    _dir: tempfile::TempDir,
}

impl World {
    /// A bridge forwarding to a fresh test gateway, a driver joined to it, and a pool on `range`.
    async fn new(range: &str) -> World {
        World::with(range, start_gateway(), BridgeSpec::default()).await
    }

    /// A bridge built from `spec` forwarding to the test gateway on `port`, a driver joined to
    /// it, and a pool on `range`.
    async fn with(range: &str, port: u16, spec: BridgeSpec) -> World {
        let dir = tempfile::tempdir().expect("dir");
        let db: PathBuf = dir.path().join("kestrel.db");
        let instance = unique();
        let cleanup = Arc::new(Cleanup::new(&instance));
        let bridge_name = GatewayBridge::container_name(&GatewayBridge::instance_id(&db));
        cleanup.container(&bridge_name);
        let bridge = within(
            "bridge start",
            GatewayBridge::start(Arc::new(ProcessRunner), port, spec, &db),
        )
        .await
        .expect("bridge starts");
        let driver = GvisorDriver::new(Arc::new(ProcessRunner))
            .with_instance(instance.clone())
            .with_bridge(bridge_name.clone());
        let pool = SubnetPool::parse(range).expect("pool");
        pool.exclude(
            &docker_subnets(&ProcessRunner)
                .await
                .expect("docker subnets"),
        );
        pool.exclude(&host_routes(&ProcessRunner).await);
        World {
            driver: Arc::new(driver),
            pool: Arc::new(pool),
            bridge,
            bridge_name,
            port,
            instance,
            cleanup,
            _dir: dir,
        }
    }

    /// A run id and its prepared network.
    async fn prepare(&self) -> (RunId, PreparedNetwork) {
        prepare(&self.driver, &self.pool, &self.cleanup).await
    }

    fn gateway_url(&self, net: &PreparedNetwork) -> String {
        format!("http://{}:{}/", net.gateway_host, self.port)
    }

    fn spec(&self, run: RunId, net: &PreparedNetwork, script: &str) -> LaunchSpec {
        spec(&self.instance, self.port, run, net, script)
    }

    async fn release(&self, net: &PreparedNetwork) {
        within("release", self.driver.release_network(net))
            .await
            .expect("release");
    }

    async fn finish(self) {
        self.bridge.stop().await;
    }
}

async fn prepare(
    driver: &GvisorDriver,
    pool: &SubnetPool,
    cleanup: &Cleanup,
) -> (RunId, PreparedNetwork) {
    let run = RunId::new();
    let subnet = pool.reserve().expect("a free block");
    let net = within("prepare", driver.prepare_network(run, &subnet))
        .await
        .expect("network prepared")
        .expect("a driver with a bridge makes a run network");
    cleanup.network(&net.name);
    assert_eq!(net.subnet, subnet);
    (run, net)
}

fn spec(instance: &str, port: u16, run: RunId, net: &PreparedNetwork, script: &str) -> LaunchSpec {
    LaunchSpec {
        image: IMAGE.to_string(),
        argv: vec!["sh".into(), "-c".into(), script.into()],
        env: BTreeMap::new(),
        workspace: None,
        config: None,
        cpu_millis: 500,
        memory_mb: 128,
        network: NetworkPolicy::RunNetwork {
            network: net.name.clone(),
            gateway: Url::parse(&format!("http://{}:{port}/", net.gateway_host)).expect("url"),
        },
        labels: BTreeMap::from([
            (RUN_LABEL.to_string(), run.to_string()),
            (INSTANCE_LABEL.to_string(), instance.to_string()),
        ]),
    }
}

/// Creates, starts and waits for a sandbox, then destroys it. Its output lines and exit code.
async fn run_to_end(
    driver: &GvisorDriver,
    cleanup: &Cleanup,
    spec: &LaunchSpec,
) -> (Vec<String>, i32) {
    within("sandbox run", async {
        let h = driver.create(spec).await.expect("create");
        cleanup.container(&h.id);
        let lines: Vec<String> = driver
            .start(&h)
            .await
            .expect("start")
            .map(|l| l.line)
            .collect()
            .await;
        let code = driver.wait(&h).await.expect("wait").code;
        driver.destroy(&h).await.expect("destroy");
        (lines, code)
    })
    .await
}

/// A shell line that prints `<name>=open` when `url` answers and `<name>=closed` when it does not.
fn probe(name: &str, url: &str) -> String {
    format!("if wget -q -T 5 -O- {url}; then echo {name}=open; else echo {name}=closed; fi")
}

fn has(lines: &[String], needle: &str) -> bool {
    lines.iter().any(|l| l.contains(needle))
}

// 4 -----------------------------------------------------------------------------------------------

/// #34 scenario 4 and #10 scenario 8: a sandbox cannot connect to a second running sandbox, while
/// its call through the gateway still works.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s4_a_sandbox_cannot_reach_another_sandbox_but_reaches_the_gateway() {
    let _serial = SERIAL.lock().await;
    let w = World::new("10.231.4.0/24").await;

    // B listens on 8080 with a minimal HTTP answer.
    let (rb, nb) = w.prepare().await;
    let listener = "while true; do printf 'HTTP/1.0 200 OK\\r\\nContent-Length: 8\\r\\nConnection: close\\r\\n\\r\\npeer-ok\\n' | nc -l -p 8080; done";
    let b = within("create b", w.driver.create(&w.spec(rb, &nb, listener)))
        .await
        .expect("create b");
    w.cleanup.container(&b.id);
    let mut output = within("start b", w.driver.start(&b))
        .await
        .expect("start b");
    let drain = tokio::spawn(async move { while output.next().await.is_some() {} });
    let b_ip = address_on(&b.id, &nb.name).await;

    // Positive control: B's listener answers on its own network, so a refusal below is isolation.
    let control = format!("http://{b_ip}:8080/");
    let mut reached = false;
    for _ in 0..30 {
        let out = docker(&[
            "run",
            "--rm",
            "--label",
            &format!("{INSTANCE_LABEL}={}", w.instance),
            "--network",
            &nb.name,
            IMAGE,
            "wget",
            "-q",
            "-T",
            "3",
            "-O-",
            &control,
        ]);
        if String::from_utf8_lossy(&out.stdout).contains("peer-ok") {
            reached = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(reached, "B's listener never answered on its own network");

    // A, on its own network, tries both.
    let (ra, na) = w.prepare().await;
    assert_ne!(na.subnet, nb.subnet);
    let script = format!(
        "{}; {}",
        probe("gw", &w.gateway_url(&na)),
        probe("peer", &control)
    );
    let (lines, _) = run_to_end(&w.driver, &w.cleanup, &w.spec(ra, &na, &script)).await;
    assert!(has(&lines, "gw=open") && has(&lines, BODY), "{lines:?}");
    assert!(has(&lines, "peer=closed"), "{lines:?}");
    assert!(!has(&lines, "peer-ok"), "{lines:?}");

    drain.abort();
    within("destroy b", w.driver.destroy(&b))
        .await
        .expect("destroy b");
    for net in [&na, &nb] {
        w.release(net).await;
        assert!(!network_exists(&net.name), "{} is removed", net.name);
        w.release(net).await;
    }
    w.finish().await;
}

// 5 -----------------------------------------------------------------------------------------------

/// #34 scenario 5 and #10 scenarios 2 and 3: the gateway answers, nothing else does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s5_a_sandbox_reaches_the_gateway_and_nothing_else() {
    let _serial = SERIAL.lock().await;
    let w = World::new("10.231.5.0/24").await;
    let docker0 = docker_ok(&[
        "network",
        "inspect",
        "bridge",
        "--format",
        "{{(index .IPAM.Config 0).Gateway}}",
    ]);
    let (run, net) = w.prepare().await;
    let script = [
        probe("gw", &w.gateway_url(&net)),
        probe("internet", "http://1.1.1.1/"),
        probe("dns", "http://example.com/"),
        probe("host", &format!("http://{docker0}:{}/", w.port)),
    ]
    .join("; ");
    let (lines, _) = run_to_end(&w.driver, &w.cleanup, &w.spec(run, &net, &script)).await;
    assert!(has(&lines, "gw=open") && has(&lines, BODY), "{lines:?}");
    for closed in ["internet=closed", "dns=closed", "host=closed"] {
        assert!(has(&lines, closed), "{closed}: {lines:?}");
    }
    w.release(&net).await;
    w.finish().await;
}

/// #34 scenario 5, the host side of the run network: docker's gateway address of the block must
/// not lead to services listening on the host. The test gateway listens on all interfaces.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s5_the_run_networks_own_gateway_address_does_not_reach_host_services() {
    let _serial = SERIAL.lock().await;
    let w = World::new("10.231.15.0/24").await;
    let (run, net) = w.prepare().await;
    let script = [
        probe("gw", &w.gateway_url(&net)),
        probe(
            "hostgw",
            &format!("http://{}:{}/", net.subnet.gateway(), w.port),
        ),
    ]
    .join("; ");
    let (lines, _) = run_to_end(&w.driver, &w.cleanup, &w.spec(run, &net, &script)).await;
    assert!(has(&lines, "gw=open"), "{lines:?}");
    assert!(has(&lines, "hostgw=closed"), "{lines:?}");
    w.release(&net).await;
    w.finish().await;
}

// 6 -----------------------------------------------------------------------------------------------

fn subnet_of(network: &str) -> String {
    docker_ok(&[
        "network",
        "inspect",
        network,
        "--format",
        "{{range .IPAM.Config}}{{.Subnet}} {{end}}",
    ])
}

fn networks_of(container: &str) -> Vec<String> {
    let mut names: Vec<String> = docker_ok(&[
        "inspect",
        "--format",
        "{{range $name, $_ := .NetworkSettings.Networks}}{{$name}} {{end}}",
        container,
    ])
    .split_whitespace()
    .map(String::from)
    .collect();
    names.sort();
    names
}

/// #34 scenario 6: 8 sandboxes created in parallel get 8 distinct subnets, each on its own
/// network only.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn s6_eight_sandboxes_created_in_parallel_get_distinct_subnets() {
    let _serial = SERIAL.lock().await;
    let w = World::new("10.231.6.0/24").await;
    let tasks: Vec<_> = (0..8)
        .map(|_| {
            let driver = w.driver.clone();
            let pool = w.pool.clone();
            let cleanup = w.cleanup.clone();
            let (instance, port) = (w.instance.clone(), w.port);
            tokio::spawn(async move {
                let (run, net) = prepare(&driver, &pool, &cleanup).await;
                let h = within(
                    "create",
                    driver.create(&spec(&instance, port, run, &net, "sleep 300")),
                )
                .await
                .expect("create");
                cleanup.container(&h.id);
                (net, h)
            })
        })
        .collect();
    let mut made: Vec<(PreparedNetwork, SandboxHandle)> = Vec::new();
    for t in tasks {
        made.push(t.await.expect("task"));
    }

    let subnets: HashSet<String> = made.iter().map(|(n, _)| subnet_of(&n.name)).collect();
    assert_eq!(subnets.len(), 8, "{subnets:?}");
    for (net, h) in &made {
        assert_eq!(subnet_of(&net.name), net.subnet.cidr().to_string());
        assert_eq!(
            networks_of(&h.id),
            vec![net.name.clone()],
            "only its own network"
        );
    }
    for (net, h) in &made {
        within("destroy", w.driver.destroy(h))
            .await
            .expect("destroy");
        w.release(net).await;
    }
    w.finish().await;
}

/// Acceptance of #34: a 10-sandbox parallel start, every sandbox reaching the gateway.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn ten_sandboxes_started_in_parallel_all_reach_the_gateway() {
    let _serial = SERIAL.lock().await;
    let w = World::new("10.231.10.0/24").await;
    let tasks: Vec<_> = (0..10)
        .map(|_| {
            let driver = w.driver.clone();
            let pool = w.pool.clone();
            let cleanup = w.cleanup.clone();
            let (instance, port) = (w.instance.clone(), w.port);
            tokio::spawn(async move {
                let (run, net) = prepare(&driver, &pool, &cleanup).await;
                let url = format!("http://{}:{port}/", net.gateway_host);
                let script = format!("wget -q -T 20 -O- {url}");
                let (lines, code) = run_to_end(
                    &driver,
                    &cleanup,
                    &spec(&instance, port, run, &net, &script),
                )
                .await;
                driver.release_network(&net).await.expect("release");
                (net, lines, code)
            })
        })
        .collect();
    let mut subnets = HashSet::new();
    for t in tasks {
        let (net, lines, code) = t.await.expect("task");
        assert_eq!(code, 0, "{}: {lines:?}", net.name);
        assert!(has(&lines, BODY), "{}: {lines:?}", net.name);
        assert!(!network_exists(&net.name), "{} is removed", net.name);
        subnets.insert(net.subnet.cidr());
    }
    assert_eq!(subnets.len(), 10);
    w.finish().await;
}

// #80 ---------------------------------------------------------------------------------------------

/// #80 scenario 2 on a real bridge: sized for 40 waiting calls and one running run (40 + 1 * 4 =
/// 44 connections, the formula `kestrel serve` uses), a sandbox holds 40 connections open through
/// it, more than the old fixed limit of 32, and one more request still goes through. With the old
/// limit the 33rd connection would wait for a free slot, so the count never reaches 40 and the
/// last request times out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn more_held_connections_than_the_old_limit_of_32_still_leave_room_for_a_call() {
    let _serial = SERIAL.lock().await;
    let held = Arc::new(AtomicUsize::new(0));
    let port = start_holding_gateway(held.clone());
    let spec = BridgeSpec {
        connections: 44,
        ..BridgeSpec::default()
    };
    let w = World::with("10.231.11.0/24", port, spec).await;
    let (run, net) = w.prepare().await;
    let url = w.gateway_url(&net);
    let script = format!(
        "i=0; while [ $i -lt 40 ]; do wget -q -T 200 -O /dev/null {url}hold & i=$((i+1)); done; \
         n=0; t=0; while [ $t -lt 90 ]; do n=$(wget -q -T 5 -O- {url}count); \
         [ \"$n\" = 40 ] && break; sleep 1; t=$((t+1)); done; echo held=$n; {}",
        probe("after", &url)
    );
    let (lines, code) = run_to_end(&w.driver, &w.cleanup, &w.spec(run, &net, &script)).await;
    assert_eq!(code, 0, "{lines:?}");
    assert!(has(&lines, "held=40"), "40 connections held open: {lines:?}");
    assert!(has(&lines, "after=open"), "one more call goes through: {lines:?}");
    assert!(has(&lines, BODY), "{lines:?}");
    assert_eq!(held.load(Ordering::SeqCst), 40);
    w.release(&net).await;
    w.finish().await;
}

// 7 -----------------------------------------------------------------------------------------------

/// #34 scenario 7 and #10 scenario 6: after a crash, the next start removes the labelled networks
/// of this instance (even one the bridge is still joined to) and leaves unlabelled networks and
/// other instances' networks alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s7_the_next_start_sweeps_labelled_leftovers_only() {
    let _serial = SERIAL.lock().await;
    let w = World::new("10.231.7.0/24").await;

    // Left by a crash: prepared, joined to the bridge, never released.
    let (_run, crashed) = w.prepare().await;
    // Labelled by hand, as an older crash would have left it.
    let block = |pool: &SubnetPool| pool.reserve().expect("block").cidr().to_string();
    let tail = |run: RunId| {
        let id = run.to_string();
        id[id.len() - 12..].to_string()
    };
    let ours = format!("kestrel-gw-{}", tail(RunId::new()));
    let theirs = format!("kestrel-gw-{}", tail(RunId::new()));
    let plain = format!("kestrel-test-plain-{}", w.instance);
    let other_instance = format!("{}-other", w.instance);
    for name in [&ours, &theirs, &plain] {
        w.cleanup.network(name);
    }
    let run_label = format!("{RUN_LABEL}={}", RunId::new());
    docker_ok(&[
        "network",
        "create",
        "--internal",
        "--subnet",
        &block(&w.pool),
        "--label",
        &run_label,
        "--label",
        &format!("{INSTANCE_LABEL}={}", w.instance),
        &ours,
    ]);
    docker_ok(&[
        "network",
        "create",
        "--internal",
        "--subnet",
        &block(&w.pool),
        "--label",
        &run_label,
        "--label",
        &format!("{INSTANCE_LABEL}={other_instance}"),
        &theirs,
    ]);
    docker_ok(&[
        "network",
        "create",
        "--internal",
        "--subnet",
        &block(&w.pool),
        "--label",
        &format!("kestrel.test={}", w.instance),
        &plain,
    ]);

    // The next start: a fresh driver for the same instance and bridge.
    let next = GvisorDriver::new(Arc::new(ProcessRunner))
        .with_instance(w.instance.clone())
        .with_bridge(w.bridge_name.clone());
    within("sweep", next.sweep_networks()).await.expect("swept");

    assert!(
        !network_exists(&crashed.name),
        "the crashed run's network is removed"
    );
    assert!(!network_exists(&ours), "our labelled leftover is removed");
    assert!(network_exists(&theirs), "another instance's network stays");
    assert!(network_exists(&plain), "an unlabelled network stays");

    let _ = docker(&["network", "rm", &theirs, &plain]);
    w.finish().await;
}

// 8 -----------------------------------------------------------------------------------------------

/// #34 scenario 8: with the docker daemon's default address pools used up by other networks,
/// runs still start, because their networks come from Kestrel's own pool.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s8_runs_start_when_the_daemon_default_pools_are_exhausted() {
    let _serial = SERIAL.lock().await;
    // The bridge needs one default-pool network, so it starts before the pools are used up.
    let w = World::new("10.231.8.0/24").await;

    let mut exhausted = false;
    for i in 0..64 {
        let name = format!("kestrel-test-fill-{}-{i}", w.instance);
        w.cleanup.network(&name);
        let out = docker(&[
            "network",
            "create",
            "--label",
            &format!("kestrel.test={}", w.instance),
            &name,
        ]);
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                stderr.contains("all predefined address pools have been fully subnetted"),
                "unexpected failure filling the pools: {stderr}"
            );
            exhausted = true;
            break;
        }
    }
    assert!(exhausted, "64 networks did not use up the default pools");

    let (run, net) = w.prepare().await;
    let script = format!("wget -q -T 20 -O- {}", w.gateway_url(&net));
    let (lines, code) = run_to_end(&w.driver, &w.cleanup, &w.spec(run, &net, &script)).await;
    assert_eq!(code, 0, "{lines:?}");
    assert!(has(&lines, BODY), "{lines:?}");
    w.release(&net).await;
    w.finish().await;
}
