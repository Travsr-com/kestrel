//! Messages a person sends to a running agent (#95).
//!
//! A message is a `MessageSent` event of its run. The agent takes the ones not yet delivered with
//! `kestrel.messages`, which records a `MessageDelivered` event naming their `seq`s in the same
//! transaction, so each message is handed over once however many calls race.

use std::collections::HashSet;

use kestrel_types::{
    AgentSpec, Event, EventKind, MESSAGES_TOOL, RunId, RunStatus, message_problem,
};
use rusqlite::{OptionalExtension, TransactionBehavior};

use crate::{Result, RunRow, Store, StoreError, ensure_run, insert_event, status_of};

/// A message waiting for its agent, as [`Store::take_messages`] hands it over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxMessage {
    /// The `seq` of its `MessageSent` event.
    pub seq: u64,
    pub text: String,
}

/// Why a run in `status`, of an agent with `spec`, cannot take a message, or `None` when it can.
/// An ended run says so first; then an agent that never takes messages; then a status other than
/// running.
fn refusal(status: &RunStatus, spec: Option<&AgentSpec>) -> Option<String> {
    let name = status.name();
    if status.is_terminal() {
        return Some(format!(
            "the run has ended ({name}), so it cannot take a message"
        ));
    }
    if !spec.is_some_and(AgentSpec::takes_messages) {
        return Some(format!(
            "its agent cannot take messages: {MESSAGES_TOOL} is not in its capabilities (mcp), \
             so nothing in its sandbox would read them"
        ));
    }
    if *status != RunStatus::Running {
        return Some(format!(
            "a {name} run cannot take a message, only a running one"
        ));
    }
    None
}

impl Store {
    /// Whether `run`'s agent takes messages (its spec grants `kestrel.messages`), whatever its
    /// status. `false` for an unknown run.
    pub fn takes_messages(&self, run: RunId) -> Result<bool> {
        Ok(self
            .run_spec(run)?
            .is_some_and(|spec| spec.takes_messages()))
    }

    /// Why `run` cannot take a message now, or `None` when it can: it must be running, and its
    /// agent must take messages. The same rule [`Store::send_message`] applies.
    pub fn message_refusal(&self, run: &RunRow) -> Result<Option<String>> {
        Ok(refusal(&run.status, self.run_spec(run.id)?.as_ref()))
    }

    /// Stores `text` as a message to `run` and returns its `MessageSent` event. Refused with
    /// [`StoreError::MessageRefused`] and the reason, writing nothing, when the text is not a
    /// message ([`message_problem`]) or the run cannot take one ([`Store::message_refusal`]). The
    /// status is checked in the same transaction as the write.
    pub fn send_message(&self, run: RunId, text: &str) -> Result<Event> {
        if let Some(problem) = message_problem(text) {
            return Err(StoreError::MessageRefused(problem));
        }
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_run(&tx, run)?;
        let status = status_of(&tx, run)?;
        let spec: Option<String> = tx
            .query_row(
                "SELECT spec FROM run_specs WHERE run_id = ?1",
                [run.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        let spec: Option<AgentSpec> = spec.map(|s| serde_json::from_str(&s)).transpose()?;
        if let Some(why) = refusal(&status, spec.as_ref()) {
            return Err(StoreError::MessageRefused(why));
        }
        let event = insert_event(
            &tx,
            run,
            EventKind::MessageSent {
                text: text.to_string(),
            },
        )?;
        tx.commit()?;
        Ok(event)
    }

    /// The messages of `run` its agent has not taken yet, oldest first, now recorded as delivered
    /// by one `MessageDelivered` event in the same transaction. Each message is returned by exactly
    /// one call. Empty, and nothing written, when there is none.
    pub fn take_messages(&self, run: RunId) -> Result<Vec<InboxMessage>> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let bodies = {
            let mut stmt = tx.prepare(
                "SELECT body FROM events WHERE run_id = ?1
                   AND json_extract(body, '$.kind.kind') IN ('message_sent', 'message_delivered')
                 ORDER BY seq",
            )?;
            stmt.query_map([run.to_string()], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut sent = Vec::new();
        let mut delivered = HashSet::new();
        for body in bodies {
            let event: Event = serde_json::from_str(&body)?;
            match event.kind {
                EventKind::MessageSent { text } => sent.push(InboxMessage {
                    seq: event.seq,
                    text,
                }),
                EventKind::MessageDelivered { seqs } => delivered.extend(seqs),
                _ => {}
            }
        }
        sent.retain(|m| !delivered.contains(&m.seq));
        if !sent.is_empty() {
            let seqs = sent.iter().map(|m| m.seq).collect();
            insert_event(&tx, run, EventKind::MessageDelivered { seqs })?;
            tx.commit()?;
        }
        Ok(sent)
    }
}
