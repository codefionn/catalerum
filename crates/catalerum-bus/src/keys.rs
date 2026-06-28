//! Key-space helpers. All bus keys are namespaced under `cat:` so a shared
//! Valkey can host other tenants. None of these keys are a source of truth.

use catalerum_core::{ConversationId, MessageId};

/// Identifies a single LLM turn (one assistant message being generated) within
/// a conversation. Token deltas for a turn flow over one channel / stream.
///
/// Cheap to clone; carries the two IDs that key the per-turn channel.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TurnId {
    /// The conversation this turn belongs to.
    pub conversation_id: ConversationId,
    /// The assistant message being streamed.
    pub message_id: MessageId,
}

impl TurnId {
    /// Construct a turn id from its conversation and message ids.
    pub const fn new(conversation_id: ConversationId, message_id: MessageId) -> Self {
        Self {
            conversation_id,
            message_id,
        }
    }
}

impl std::fmt::Display for TurnId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.conversation_id, self.message_id)
    }
}

/// Pub/sub channel name for a turn's token stream: `cat:turn:{conv}:{msg}`.
pub fn turn_channel(turn: &TurnId) -> String {
    format!("cat:turn:{}:{}", turn.conversation_id, turn.message_id)
}

/// Redis Stream key for a turn's **replayable** frame buffer (SOUL §7/§12
/// detached streaming): `cat:turnbuf:{conv}:{msg}`. Unlike [`turn_channel`]
/// (pub/sub, no backlog) this stream retains entries so a reconnecting client
/// resumes from its last-seen id with no gap. Keyed by turn, TTL'd, throwaway.
pub fn turn_buffer_key(turn: &TurnId) -> String {
    format!("cat:turnbuf:{}:{}", turn.conversation_id, turn.message_id)
}

/// Redis Stream name (for the [`crate::WorkQueue`]) carrying a conversation's
/// **durable** mid-turn user input (SOUL §12): `convin:{conversation_id}`. The
/// pod holding a client socket pushes a mid-turn "say" here; the pod running the
/// detached turn consumes it, so an injected message survives a cross-pod hop
/// (pub/sub would drop it). Not a source of truth — acked once injected.
pub fn conv_input_stream(conversation_id: &str) -> String {
    format!("convin:{conversation_id}")
}

/// Stream key for a named work queue: `cat:stream:{name}`.
pub fn stream_key(name: &str) -> String {
    format!("cat:stream:{name}")
}

/// Pub/sub channel for cross-pod MCP `GET /mcp` server→client push fan-out
/// (SOUL §26): `cat:mcp:push`. A **single** channel carries every workspace's
/// push; the payload envelope (owned by the publisher) carries the target
/// workspace + the originating-pod nonce, so one subscriber task per pod suffices
/// and a pod skips re-broadcasting its own relayed message.
#[must_use]
pub fn mcp_push_channel() -> &'static str {
    "cat:mcp:push"
}

/// Lock key for a named resource: `cat:lock:{name}`.
pub fn lock_key(name: &str) -> String {
    format!("cat:lock:{name}")
}

/// Registry key for a pod's reachability announcement: `cat:pod:{pod_id}`.
/// The value is the JSON envelope owned by the API layer (currently the pod's
/// in-cluster `host:port`), announced on the heartbeat clock so a dead pod's
/// entry lapses on its own (SOUL §16 M7 cross-pod session routing).
pub fn pod_key(pod_id: &str) -> String {
    format!("cat:pod:{pod_id}")
}

/// Pub/sub channel for a conversation's cross-pod **turn control** (SOUL §12/§16
/// M7): `cat:conv:ctl:{conversation_id}`. The pod streaming a turn for the
/// conversation subscribes for the turn's duration; any pod that receives a
/// control action for a turn it isn't running (a stop from a peer-pod client)
/// publishes here. Best-effort like every push channel — a lost stop costs one
/// click, never data.
pub fn conv_ctl_channel(conversation_id: &str) -> String {
    format!("cat:conv:ctl:{conversation_id}")
}
