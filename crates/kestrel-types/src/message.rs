//! Messages a person sends to a running agent (#95). The agent takes them with the
//! [`MESSAGES_TOOL`] Kestrel tool, which its spec must grant.

use crate::AgentSpec;

/// The Kestrel tool an agent reads its messages with. A run takes messages only when its spec
/// grants it (in `capabilities.mcp`).
pub const MESSAGES_TOOL: &str = "kestrel.messages";

/// The most bytes one message may hold: 16 KiB.
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024;

/// Why `text` cannot be sent as a message, or `None` when it can. A message is text of 1 to
/// [`MAX_MESSAGE_BYTES`] bytes that is not only whitespace, with no control character other than
/// a line break or a tab, so it cannot move a terminal's cursor when `kestrel watch` prints it.
pub fn message_problem(text: &str) -> Option<String> {
    if text.trim().is_empty() {
        return Some("the message is empty".into());
    }
    if text.len() > MAX_MESSAGE_BYTES {
        return Some(format!(
            "the message is {} bytes, and one may hold at most {MAX_MESSAGE_BYTES}",
            text.len()
        ));
    }
    if text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return Some(
            "a message is plain text: no control characters other than line breaks and tabs".into(),
        );
    }
    None
}

impl AgentSpec {
    /// Whether runs of this agent take messages from a person: its capabilities grant
    /// [`MESSAGES_TOOL`].
    pub fn takes_messages(&self) -> bool {
        self.capabilities.mcp.iter().any(|t| t == MESSAGES_TOOL)
    }
}
