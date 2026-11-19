//! Test only: a sandbox driver that starts nothing. Compiled into debug builds only, so release
//! binaries never contain it.

use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use kestrel_sandbox::{
    ExitStatus, LaunchSpec, OutputLine, OutputStream, Result, SandboxDriver, SandboxHandle,
    StdStream,
};

/// The one line every fake sandbox prints: a successful Claude Code `result`.
const RESULT_LINE: &str =
    r#"{"type":"result","subtype":"success","is_error":false,"result":"fake run done"}"#;

/// Named `fake`, counts as strong isolation, and every sandbox exits 0 after [`RESULT_LINE`].
#[derive(Debug, Default)]
pub(crate) struct FakeDriver {
    next: AtomicU64,
}

#[async_trait]
impl SandboxDriver for FakeDriver {
    fn name(&self) -> &'static str {
        "fake"
    }
    fn is_strong(&self) -> bool {
        true
    }
    async fn create(&self, _spec: &LaunchSpec) -> Result<SandboxHandle> {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        Ok(SandboxHandle {
            id: format!("fake-{n}"),
        })
    }
    async fn start(&self, _h: &SandboxHandle) -> Result<OutputStream> {
        let line = OutputLine {
            stream: StdStream::Stdout,
            line: RESULT_LINE.to_string(),
        };
        Ok(Box::pin(futures::stream::iter([line])))
    }
    async fn wait(&self, _h: &SandboxHandle) -> Result<ExitStatus> {
        Ok(ExitStatus { code: 0 })
    }
    async fn stop(&self, _h: &SandboxHandle) -> Result<()> {
        Ok(())
    }
    async fn destroy(&self, _h: &SandboxHandle) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use kestrel_sandbox::NetworkPolicy;

    #[tokio::test]
    async fn every_sandbox_prints_one_result_line_and_exits_0() {
        let d = FakeDriver::default();
        assert_eq!(d.name(), "fake");
        assert!(d.is_strong());
        let spec = LaunchSpec {
            image: "img".into(),
            argv: vec![],
            env: Default::default(),
            workspace: None,
            cpu_millis: 1000,
            memory_mb: 512,
            network: NetworkPolicy::None,
        };
        let a = d.create(&spec).await.expect("create");
        let b = d.create(&spec).await.expect("create");
        assert_ne!(a, b);
        let lines: Vec<OutputLine> = d.start(&a).await.expect("start").collect().await;
        assert_eq!(lines.len(), 1);
        assert!(lines[0].line.contains("\"result\""));
        assert!(d.wait(&a).await.expect("wait").success());
        d.stop(&a).await.expect("stop");
        d.destroy(&a).await.expect("destroy");
    }
}
