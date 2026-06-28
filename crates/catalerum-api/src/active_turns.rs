//! Process-level registry of **detached** chat turns (SOUL §7/§12).
//!
//! When a chat turn is decoupled from its client socket it runs in a spawned
//! task, not on the connection. This registry lets the pod that owns the run
//! answer two questions cheaply, without a Valkey round-trip:
//!
//! * *Is a turn for conversation X running here, and what is its [`TurnId`]?* —
//!   so a reconnecting socket that landed on this pod attaches straight to the
//!   live buffer (the cross-pod fallback is the Valkey active-turn Registry key).
//! * *Cancel it* — a Stop read off a local socket cancels the run instantly
//!   (the cross-pod fallback is the conversation control channel).
//!
//! It is **not** a source of truth: an empty registry costs a Valkey lookup,
//! never correctness. Mid-turn user input is delivered durably over the
//! Streams work queue, so it does not live here.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use catalerum_bus::TurnId;
use catalerum_core::ConversationId;
use catalerum_llm::CancellationToken;

#[derive(Default)]
struct Inner {
    /// Every live turn on this pod → its cancel handle.
    by_turn: HashMap<TurnId, CancellationToken>,
    /// Conversation → its live turns (usually one; two only across the brief
    /// window where a queued turn is registered before the prior one clears).
    by_conv: HashMap<ConversationId, HashSet<TurnId>>,
}

/// Cheap-to-clone handle to the pod's live detached turns.
#[derive(Clone, Default)]
pub struct ActiveTurns {
    inner: Arc<Mutex<Inner>>,
}

impl ActiveTurns {
    /// Empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn register(&self, turn: TurnId, cancel: CancellationToken) {
        let mut inner = self.inner.lock().expect("active-turns mutex poisoned");
        inner.by_turn.insert(turn, cancel);
        inner
            .by_conv
            .entry(turn.conversation_id)
            .or_default()
            .insert(turn);
    }

    fn deregister(&self, turn: &TurnId) {
        let mut inner = self.inner.lock().expect("active-turns mutex poisoned");
        inner.by_turn.remove(turn);
        if let Some(set) = inner.by_conv.get_mut(&turn.conversation_id) {
            set.remove(turn);
            if set.is_empty() {
                inner.by_conv.remove(&turn.conversation_id);
            }
        }
    }

    /// Register `turn` and return an RAII guard that deregisters it on drop —
    /// firing on the spawned task's success, error, and panic alike.
    #[must_use]
    pub fn register_guarded(&self, turn: TurnId, cancel: CancellationToken) -> ActiveTurnGuard {
        self.register(turn, cancel);
        ActiveTurnGuard {
            turns: self.clone(),
            turn,
        }
    }

    /// The live turn for `conversation` on this pod, if any (attach fast-path).
    #[must_use]
    pub fn active_turn_for(&self, conversation: ConversationId) -> Option<TurnId> {
        let inner = self.inner.lock().expect("active-turns mutex poisoned");
        inner
            .by_conv
            .get(&conversation)
            .and_then(|set| set.iter().next().copied())
    }

    /// Cancel every live turn for `conversation` on this pod. Returns `true` if
    /// at least one turn was found and signalled (so a local Stop can skip the
    /// cross-pod control-channel publish).
    pub fn cancel_conversation(&self, conversation: ConversationId) -> bool {
        let inner = self.inner.lock().expect("active-turns mutex poisoned");
        let Some(turns) = inner.by_conv.get(&conversation) else {
            return false;
        };
        let mut hit = false;
        for turn in turns {
            if let Some(cancel) = inner.by_turn.get(turn) {
                cancel.cancel();
                hit = true;
            }
        }
        hit
    }
}

/// Deregisters its turn from [`ActiveTurns`] when dropped. Held by the spawned
/// run task for its whole lifetime.
pub struct ActiveTurnGuard {
    turns: ActiveTurns,
    turn: TurnId,
}

impl Drop for ActiveTurnGuard {
    fn drop(&mut self) {
        self.turns.deregister(&self.turn);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use catalerum_core::MessageId;

    #[test]
    fn guard_deregisters_and_cancel_walks_conversation() {
        let turns = ActiveTurns::new();
        let conv = ConversationId::new();
        let turn = TurnId::new(conv, MessageId::new());
        let cancel = CancellationToken::new();

        let guard = turns.register_guarded(turn, cancel.clone());
        assert_eq!(turns.active_turn_for(conv), Some(turn));
        assert!(turns.cancel_conversation(conv));
        assert!(cancel.is_cancelled());

        drop(guard);
        assert_eq!(turns.active_turn_for(conv), None);
        assert!(!turns.cancel_conversation(conv));
    }
}
