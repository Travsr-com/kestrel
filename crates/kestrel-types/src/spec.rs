//! Agent spec and its parts.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::MicroUsd;

#[derive(Debug, thiserror::Error)]
pub enum SpecError {
    #[error("{0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Runtime {
    #[serde(rename = "claude-code")]
    ClaudeCode,
    #[serde(rename = "opencode")]
    OpenCode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tier {
    #[serde(rename = "tier:cheap")]
    Cheap,
    #[serde(rename = "tier:mid")]
    Mid,
    #[serde(rename = "tier:frontier")]
    Frontier,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxTier {
    Auto,
    Gvisor,
    AppleContainer,
    DockerRunc,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkspaceSpec {
    GitWorktree { repo: PathBuf },
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
pub struct SpawnPolicy {
    pub max_children: u32,
    pub max_depth: u32,
    pub agents: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub mcp: Vec<String>,
    pub network: Vec<String>,
    pub secrets: Vec<String>,
    pub spawn: Option<SpawnPolicy>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approvals {
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
}

impl AgentSpec {
    pub fn from_yaml(_yaml: &str) -> Result<AgentSpec, SpecError> {
        todo!()
    }
}
