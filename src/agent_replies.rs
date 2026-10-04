//! Replies an agent's own session suggests for its final message.
//!
//! A reporter, usually a hook on the agent's harness, sends `pane.report_replies` with the final
//! message it saw and up to three replies the user is likely to send next. Herdr keeps them in
//! memory with the pane's terminal and hands them out only through `agent.last_message`, and only
//! while the message it reads from the transcript is the one they were offered for.
//!
//! "The same message" cannot mean equal text. The harness and Herdr's transcript reader join
//! content blocks, trim and escape differently, so two renderings of one message are told apart
//! from two messages by their letters and digits alone: whitespace, punctuation, markdown and the
//! way blocks are joined all drop out of the [`MessageKey`].

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The most replies one report may carry.
pub const MAX_REPLIES: usize = 3;

/// The longest a reply may be, in characters, once trimmed.
pub const MAX_REPLY_CHARS: usize = 200;

/// A message's identity: a digest of its letters and digits, in order.
///
/// Letters and digits are the characters Unicode counts as alphanumeric. Everything else is
/// dropped before hashing, so the key survives any change that only touches what is between them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MessageKey(String);

impl MessageKey {
    /// The key of `text`, or `None` when it has no letter or digit and so no identity.
    pub fn of(text: &str) -> Option<Self> {
        let mut hasher = Sha256::new();
        // The digest is fed in blocks, so a long message costs no copy of its alphanumerics.
        let mut block = [0u8; 256];
        let mut filled = 0;
        let mut any = false;
        for ch in text.chars().filter(|ch| ch.is_alphanumeric()) {
            any = true;
            if filled + ch.len_utf8() > block.len() {
                hasher.update(&block[..filled]);
                filled = 0;
            }
            filled += ch.encode_utf8(&mut block[filled..]).len();
        }
        any.then(|| {
            hasher.update(&block[..filled]);
            Self(format!("{:x}", hasher.finalize()))
        })
    }
}

/// The replies a reporter offered for one message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferedReplies {
    /// The message they answer.
    pub message: MessageKey,
    /// Trimmed and within the limits, most likely first.
    pub replies: Vec<String>,
}

impl OfferedReplies {
    /// The replies, when `message` is the one they were offered for, and none otherwise.
    pub fn replies_for(&self, message: &str) -> &[String] {
        if MessageKey::of(message).as_ref() == Some(&self.message) {
            &self.replies
        } else {
            &[]
        }
    }
}

/// What a terminal's agent has offered: the newest report, and how many reports were accepted.
#[derive(Debug, Default)]
pub struct AgentReplies {
    revision: u64,
    offered: Option<OfferedReplies>,
}

impl AgentReplies {
    /// Takes a report in. It replaces the previous one, an empty one included.
    pub fn report(&mut self, message: MessageKey, replies: Vec<String>) {
        self.offered = Some(OfferedReplies { message, replies });
        self.revision = self.revision.saturating_add(1);
    }

    /// Reports accepted so far; 0 before the first.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn offered(&self) -> Option<&OfferedReplies> {
        self.offered.as_ref()
    }
}

/// Trims each reply and checks a report's limits. The error says what to change.
pub fn normalize_replies(replies: Vec<String>) -> Result<Vec<String>, String> {
    if replies.len() > MAX_REPLIES {
        return Err(format!(
            "replies may hold at most {MAX_REPLIES} items, got {}",
            replies.len()
        ));
    }
    replies
        .into_iter()
        .enumerate()
        .map(|(index, reply)| {
            let reply = reply.trim();
            if reply.is_empty() {
                return Err(format!("replies[{index}] must not be empty"));
            }
            let chars = reply.chars().count();
            if chars > MAX_REPLY_CHARS {
                return Err(format!(
                    "replies[{index}] is {chars} characters long; at most {MAX_REPLY_CHARS} are allowed"
                ));
            }
            Ok(reply.to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests;
