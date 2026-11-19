//! OpenCode in non-interactive `run` mode with `--format json` output.

use kestrel_types::{AgentSpec, Runtime};

use crate::{LaunchPlan, ParsedLine, Result, RunContext, RuntimeAdapter};

#[derive(Debug, Default)]
pub struct OpenCodeAdapter;

impl RuntimeAdapter for OpenCodeAdapter {
    fn runtime(&self) -> Runtime {
        todo!()
    }

    fn launch_plan(&self, spec: &AgentSpec, input: &str, ctx: &RunContext) -> Result<LaunchPlan> {
        let _ = (spec, input, ctx);
        todo!()
    }

    fn parse_line(&self, line: &str) -> ParsedLine {
        let _ = line;
        todo!()
    }
}
