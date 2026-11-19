//! Agent spec and its parts.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::MicroUsd;

#[derive(Debug, thiserror::Error)]
pub enum SpecError {
    #[error("{0}")]
    Invalid(String),
    #[error("invalid agent spec: {0}")]
    Yaml(#[from] serde_saphyr::Error),
}

fn invalid(field: &str, msg: impl fmt::Display) -> SpecError {
    SpecError::Invalid(format!("{field}: {msg}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Runtime {
    #[serde(rename = "claude-code")]
    ClaudeCode,
    #[serde(rename = "opencode")]
    OpenCode,
}

impl Runtime {
    pub const ALL: [Runtime; 2] = [Runtime::ClaudeCode, Runtime::OpenCode];

    pub fn as_str(self) -> &'static str {
        match self {
            Runtime::ClaudeCode => "claude-code",
            Runtime::OpenCode => "opencode",
        }
    }
}

impl fmt::Display for Runtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Runtime {
    type Err = SpecError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Runtime::ALL
            .into_iter()
            .find(|r| r.as_str() == s)
            .ok_or_else(|| {
                let supported: Vec<&str> = Runtime::ALL.iter().map(|r| r.as_str()).collect();
                invalid(
                    "runtime",
                    format_args!(
                        "`{s}` is not supported yet (supported: {})",
                        supported.join(", ")
                    ),
                )
            })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Tier {
    #[serde(rename = "tier:cheap")]
    Cheap,
    #[serde(rename = "tier:mid")]
    Mid,
    #[serde(rename = "tier:frontier")]
    Frontier,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Cheap => "tier:cheap",
            Tier::Mid => "tier:mid",
            Tier::Frontier => "tier:frontier",
        }
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Tier {
    type Err = SpecError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        [Tier::Cheap, Tier::Mid, Tier::Frontier]
            .into_iter()
            .find(|t| t.as_str() == s)
            .ok_or_else(|| {
                invalid(
                    "model_policy",
                    format_args!(
                        "unknown tier `{s}` (expected tier:cheap, tier:mid or tier:frontier)"
                    ),
                )
            })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxTier {
    Auto,
    Gvisor,
    AppleContainer,
    DockerRunc,
}

impl FromStr for SandboxTier {
    type Err = SpecError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "auto" => Ok(SandboxTier::Auto),
            "gvisor" => Ok(SandboxTier::Gvisor),
            "apple-container" => Ok(SandboxTier::AppleContainer),
            "docker-runc" => Ok(SandboxTier::DockerRunc),
            _ => Err(invalid(
                "sandbox.tier",
                format_args!(
                    "unknown tier `{s}` (expected auto, gvisor, apple-container or docker-runc)"
                ),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspaceSpec {
    /// A git worktree of `repo`. From YAML, `repo` is `.` (the project the agent is registered in).
    GitWorktree {
        repo: PathBuf,
    },
    EmptyDir,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxSpec {
    pub tier: SandboxTier,
    pub image: Option<String>,
    pub cpu_millis: u32,
    pub memory_mb: u64,
    pub workspace: WorkspaceSpec,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    pub limit: MicroUsd,
    pub wall_clock: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPolicy {
    pub default: Tier,
    pub escalate: Option<Tier>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnPolicy {
    pub max_children: u32,
    pub max_depth: u32,
    #[serde(default)]
    pub agents: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    #[serde(default)]
    pub mcp: Vec<String>,
    #[serde(default)]
    pub network: Vec<String>,
    #[serde(default)]
    pub secrets: Vec<String>,
    #[serde(default)]
    pub spawn: Option<SpawnPolicy>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Approvals {
    #[serde(default)]
    pub required_for: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSpec {
    pub name: String,
    pub runtime: Runtime,
    pub role: String,
    pub model_policy: ModelPolicy,
    pub capabilities: Capabilities,
    pub sandbox: SandboxSpec,
    pub budget: Budget,
    pub approvals: Approvals,
    /// `apiVersion`, kept as written. Not interpreted yet.
    pub api_version: Option<String>,
    /// `kind`, kept as written. Not interpreted yet.
    pub kind: Option<String>,
    /// `memory`, kept as written. Interpreted by a later milestone.
    pub memory: Option<serde_json::Value>,
    /// `triggers`, kept as written. Interpreted by a later milestone.
    pub triggers: Vec<serde_json::Value>,
}

// YAML shape. Scalars with human units are read as their raw text so they parse exactly.

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSpec {
    #[serde(rename = "apiVersion")]
    api_version: Option<String>,
    kind: Option<String>,
    name: String,
    runtime: String,
    role: String,
    model_policy: RawModelPolicy,
    #[serde(default)]
    capabilities: Capabilities,
    sandbox: RawSandbox,
    budget: RawBudget,
    #[serde(default)]
    approvals: Approvals,
    memory: Option<serde_json::Value>,
    #[serde(default)]
    triggers: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModelPolicy {
    default: String,
    escalate: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSandbox {
    tier: String,
    image: Option<String>,
    cpu: String,
    memory: String,
    workspace: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBudget {
    usd: String,
    wall_clock: String,
}

impl AgentSpec {
    /// Parses the Agent YAML format from `docs/architecture.md` section 3.1.
    pub fn from_yaml(yaml: &str) -> Result<AgentSpec, SpecError> {
        let raw: RawSpec = serde_saphyr::from_str(yaml)?;
        if raw.name.trim().is_empty() {
            return Err(invalid("name", "must not be empty"));
        }
        let escalate = raw
            .model_policy
            .escalate
            .as_deref()
            .map(str::parse)
            .transpose()?;
        Ok(AgentSpec {
            name: raw.name,
            runtime: raw.runtime.parse()?,
            role: raw.role,
            model_policy: ModelPolicy {
                default: raw.model_policy.default.parse()?,
                escalate,
            },
            capabilities: raw.capabilities,
            sandbox: SandboxSpec {
                tier: raw.sandbox.tier.parse()?,
                image: raw.sandbox.image,
                cpu_millis: parse_cpu(&raw.sandbox.cpu)?,
                memory_mb: parse_memory(&raw.sandbox.memory)?,
                workspace: parse_workspace(&raw.sandbox.workspace)?,
            },
            budget: Budget {
                limit: parse_usd(&raw.budget.usd)?,
                wall_clock: parse_wall_clock(&raw.budget.wall_clock)?,
            },
            approvals: raw.approvals,
            api_version: raw.api_version,
            kind: raw.kind,
            memory: raw.memory,
            triggers: raw.triggers,
        })
    }
}

/// Parses a non-negative decimal like `2.05` into an integer scaled by `10^places`, exactly.
/// Returns `None` for anything else, including more precision than `places` allows.
fn parse_fixed(s: &str, places: u32) -> Option<u64> {
    let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
    let digits = |p: &str| p.bytes().all(|b| b.is_ascii_digit());
    if whole.is_empty() && frac.is_empty() || !digits(whole) || !digits(frac) {
        return None;
    }
    let frac = frac.trim_end_matches('0');
    if frac.len() > places as usize {
        return None;
    }
    let scale = 10u64.pow(places);
    let whole: u64 = if whole.is_empty() {
        0
    } else {
        whole.parse().ok()?
    };
    let frac_val: u64 = if frac.is_empty() {
        0
    } else {
        frac.parse().ok()?
    };
    let frac_val = frac_val * 10u64.pow(places - frac.len() as u32);
    whole.checked_mul(scale)?.checked_add(frac_val)
}

impl MicroUsd {
    /// Parses a non-negative dollar amount like `2.50` exactly. `None` for anything else: a sign,
    /// an exponent, more than 6 decimal places, or an amount too large to hold.
    pub fn parse_decimal(s: &str) -> Option<MicroUsd> {
        parse_fixed(s, 6)
            .and_then(|v| i64::try_from(v).ok())
            .map(MicroUsd)
    }
}

fn parse_usd(s: &str) -> Result<MicroUsd, SpecError> {
    if s.starts_with('-') {
        return Err(invalid(
            "budget.usd",
            format_args!("`{s}` must not be negative"),
        ));
    }
    MicroUsd::parse_decimal(s).ok_or_else(|| {
        invalid(
            "budget.usd",
            format_args!("`{s}` is not a dollar amount with at most 6 decimal places"),
        )
    })
}

/// `2` is two cores, `500m` is half a core.
fn parse_cpu(s: &str) -> Result<u32, SpecError> {
    let millis = match s.strip_suffix('m') {
        Some(m) => m.parse::<u64>().ok(),
        None => parse_fixed(s, 3),
    };
    match millis.and_then(|v| u32::try_from(v).ok()) {
        Some(0) => Err(invalid("sandbox.cpu", "must be greater than zero")),
        Some(v) => Ok(v),
        None => Err(invalid(
            "sandbox.cpu",
            format_args!("`{s}` is not a cpu amount (examples: 2, 0.5, 500m)"),
        )),
    }
}

/// `4Gi` is 4096 MB, `512Mi` and plain `512` are 512 MB.
fn parse_memory(s: &str) -> Result<u64, SpecError> {
    let (num, mult) = if let Some(n) = s.strip_suffix("Gi") {
        (n, 1024)
    } else {
        (s.strip_suffix("Mi").unwrap_or(s), 1)
    };
    match num.parse::<u64>().ok().and_then(|n| n.checked_mul(mult)) {
        Some(0) => Err(invalid("sandbox.memory", "must be greater than zero")),
        Some(v) => Ok(v),
        None => Err(invalid(
            "sandbox.memory",
            format_args!("`{s}` is not a memory size (examples: 512Mi, 4Gi, 512)"),
        )),
    }
}

/// `90s`, `30m`, `2h`.
fn parse_wall_clock(s: &str) -> Result<Duration, SpecError> {
    let unit = match s.chars().last() {
        Some('s') => 1,
        Some('m') => 60,
        Some('h') => 3600,
        _ => 0,
    };
    let secs = (unit > 0)
        .then(|| s[..s.len() - 1].parse::<u64>().ok())
        .flatten()
        .and_then(|n| n.checked_mul(unit));
    match secs {
        Some(0) => Err(invalid("budget.wall_clock", "must be greater than zero")),
        Some(v) => Ok(Duration::from_secs(v)),
        None => Err(invalid(
            "budget.wall_clock",
            format_args!("`{s}` is not a duration (examples: 90s, 30m, 2h)"),
        )),
    }
}

fn parse_workspace(s: &str) -> Result<WorkspaceSpec, SpecError> {
    match s {
        "git-worktree" => Ok(WorkspaceSpec::GitWorktree {
            repo: PathBuf::from("."),
        }),
        "empty-dir" => Ok(WorkspaceSpec::EmptyDir),
        _ => Err(invalid(
            "sandbox.workspace",
            format_args!("unknown workspace `{s}` (expected git-worktree or empty-dir)"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(r: Result<impl fmt::Debug, SpecError>) -> String {
        match r {
            Ok(v) => panic!("expected an error, got {v:?}"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn cpu_units() {
        assert_eq!(parse_cpu("2").ok(), Some(2000));
        assert_eq!(parse_cpu("500m").ok(), Some(500));
        assert_eq!(parse_cpu("0.5").ok(), Some(500));
        assert!(err(parse_cpu("0")).contains("sandbox.cpu"));
        assert!(err(parse_cpu("0m")).contains("greater than zero"));
        assert!(err(parse_cpu("lots")).contains("sandbox.cpu"));
        assert!(err(parse_cpu("0.0005")).contains("sandbox.cpu"));
        assert!(err(parse_cpu("99999999")).contains("sandbox.cpu"));
    }

    #[test]
    fn memory_units() {
        assert_eq!(parse_memory("512Mi").ok(), Some(512));
        assert_eq!(parse_memory("1Gi").ok(), Some(1024));
        assert_eq!(parse_memory("512").ok(), Some(512));
        assert!(err(parse_memory("0Mi")).contains("sandbox.memory"));
        assert!(err(parse_memory("4GB")).contains("sandbox.memory"));
        assert!(err(parse_memory(&format!("{}Gi", u64::MAX))).contains("sandbox.memory"));
    }

    #[test]
    fn wall_clock_units() {
        assert_eq!(parse_wall_clock("90s").ok(), Some(Duration::from_secs(90)));
        assert_eq!(
            parse_wall_clock("30m").ok(),
            Some(Duration::from_secs(1800))
        );
        assert_eq!(parse_wall_clock("2h").ok(), Some(Duration::from_secs(7200)));
        assert!(err(parse_wall_clock("0m")).contains("greater than zero"));
        assert!(err(parse_wall_clock("30")).contains("budget.wall_clock"));
        assert!(err(parse_wall_clock("xm")).contains("budget.wall_clock"));
        assert!(err(parse_wall_clock("")).contains("budget.wall_clock"));
    }

    #[test]
    fn usd_is_exact() {
        assert_eq!(parse_usd("2.00").ok(), Some(MicroUsd(2_000_000)));
        assert_eq!(parse_usd("0.1").ok(), Some(MicroUsd(100_000)));
        assert_eq!(parse_usd(".5").ok(), Some(MicroUsd(500_000)));
        assert_eq!(parse_usd("3").ok(), Some(MicroUsd(3_000_000)));
        assert_eq!(parse_usd("1.0000010").ok(), Some(MicroUsd(1_000_001)));
        assert!(err(parse_usd(".")).contains("budget.usd"));
        assert!(err(parse_usd("1e3")).contains("budget.usd"));
        assert!(err(parse_usd("99999999999999999999")).contains("budget.usd"));
    }

    #[test]
    fn sandbox_tier_spellings() {
        let cases = [
            ("auto", SandboxTier::Auto),
            ("gvisor", SandboxTier::Gvisor),
            ("apple-container", SandboxTier::AppleContainer),
            ("docker-runc", SandboxTier::DockerRunc),
        ];
        for (text, tier) in cases {
            assert_eq!(text.parse::<SandboxTier>().ok(), Some(tier));
        }
        assert!(err("firecracker".parse::<SandboxTier>()).contains("sandbox.tier"));
    }

    #[test]
    fn workspace_spellings() {
        assert_eq!(
            parse_workspace("empty-dir").ok(),
            Some(WorkspaceSpec::EmptyDir)
        );
        assert!(err(parse_workspace("tmp")).contains("sandbox.workspace"));
    }

    #[test]
    fn runtime_and_tier_display_round_trip() {
        for r in Runtime::ALL {
            assert_eq!(r.to_string().parse::<Runtime>().ok(), Some(r));
        }
        for t in [Tier::Cheap, Tier::Mid, Tier::Frontier] {
            assert_eq!(t.to_string().parse::<Tier>().ok(), Some(t));
        }
        assert!(err("tier:huge".parse::<Tier>()).contains("model_policy"));
    }

    #[test]
    fn minimal_spec_defaults_optional_sections() {
        let yaml = "name: a\nruntime: opencode\nrole: r\nmodel_policy: { default: tier:cheap }\n\
                    sandbox: { tier: gvisor, image: img, cpu: 500m, memory: 512Mi, workspace: empty-dir }\n\
                    budget: { usd: 1, wall_clock: 90s }\n";
        let spec = AgentSpec::from_yaml(yaml).ok();
        let spec = spec.as_ref();
        assert_eq!(spec.map(|s| s.model_policy.escalate), Some(None));
        assert_eq!(
            spec.map(|s| s.capabilities.clone()),
            Some(Capabilities::default())
        );
        assert_eq!(
            spec.and_then(|s| s.sandbox.image.clone()),
            Some("img".into())
        );
        assert_eq!(spec.map(|s| s.triggers.len()), Some(0));
    }

    #[test]
    fn unknown_nested_key_rejected() {
        let yaml = "name: a\nruntime: opencode\nrole: r\nmodel_policy: { default: tier:cheap }\n\
                    sandbox: { tier: auto, cpu: 1, memory: 1, workspace: empty-dir, gpu: 1 }\n\
                    budget: { usd: 1, wall_clock: 1s }\n";
        assert!(err(AgentSpec::from_yaml(yaml)).contains("gpu"));
    }
}
