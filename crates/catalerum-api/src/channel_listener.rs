//! Inbound channel listener (SOUL §11/§25) — the *receive* half wired to a live
//! long-poll, the complement of the `POST /channels/{channel}/inbound` relay route.
//!
//! For each inbound-capable channel (a Matrix `/sync` or Telegram `getUpdates`
//! channel configured with `inbound = true`, [`AppState::inbound_channels`]), this
//! spawns a task that [`Channel::subscribe`]s and, for every message that arrives,
//! dispatches a `TriggerEvent::ChannelMessage` — the same bridge
//! ([`catalerum_ingest::dispatch_trigger_event`]) the relay route, webhooks (§25),
//! and Kanban moves (§24) use. So an enabled `channel_message` automation fires, an
//! `LlmAgent` action sees the message (+ who sent it), and the action runner's
//! auto-reply delivers the agent's response back through the **same-named** channel
//! — i.e. back to the room/chat it came from. That closes the multiplayer loop:
//! several people in a room, catalerum listening and replying in place.
//!
//! Channels are static `[channels]` config, so — like the `[email]` Maildir
//! pre-seed — they bind to one workspace (the default), passed in at construction.
//!
//! **Multi-pod singleton (SOUL §6.6/§16 M7).** Every pod loads the same
//! `[channels]` config, so every pod would otherwise `subscribe` to the same
//! Matrix `/sync` / Telegram `getUpdates` and dispatch each inbound message N
//! times (each pod keeps its own in-process `since`/`offset`). To make each
//! channel a **single consumer** across pods, a per-`(workspace, channel)`
//! leader lease ([`channel_leader_key`], the bus [`DistLock`]) gates the listen
//! loop: only the lease holder subscribes + dispatches, refreshing the lease
//! while it consumes; a peer only takes over once the holder crashes/releases and
//! the lease frees. With the in-process bus (single-pod dev) the lease is
//! uncontended, so behaviour is unchanged.
//!
//! **What the lease does NOT guarantee — and the dispatch dedup that closes it.**
//! The lease is a Redis `SET NX PX` coordination hint, not a fencing oracle: if a
//! leader stalls (e.g. a GC/network pause) past [`LEADER_TTL`] while its long-poll
//! socket stays live, its lease can expire and a peer can take over — for that
//! window *two* pods may both drain the stream and dispatch the same inbound
//! message. We choose the TTL well above the poll cycle to make that rare, but it
//! is real. To make the common case **exactly-once**, [`dispatch_inbound`] claims
//! each message's provider identity before dispatching: a short-lived, never-
//! released bus lock keyed [`channel_msg_claim_key`]
//! (`channel-msg:{ws}:{channel}:{source}:{message_id}`, [`DISPATCH_CLAIM_TTL`] =
//! 10 min, generously over the fencing window). The winner dispatches; a peer that
//! finds the claim taken skips silently (debug log). The claim folds in `source`
//! (room/chat id) because a Telegram `message_id` is unique only *within a chat*,
//! not globally — without it, msg #7 from chat B would be dropped as a "duplicate"
//! of chat A's #7. Matrix's `message_id` is the globally-unique `event_id`, so the
//! `source` there is merely a harmless (constant, per-channel room) disambiguator.
//!
//! **Providers with no id → at-least-once (never drop mail).** An [`InMessage`]
//! whose `message_id` is `None` dispatches unconditionally. In practice both
//! long-poll providers populate it — Matrix from `event_id`, Telegram from
//! `message.message_id` — and the webhook channels (Discord/Slack) are
//! outbound-only, so `None` only arises if a provider omits the field; those
//! dispatch at-least-once rather than being dropped for want of an id.
//!
//! **Durability honesty.** The claim lives in Valkey (or, single-pod, the
//! in-process lock) — it is *not* backed by a Postgres ledger. If Valkey
//! **restarts inside the 10-min window** the claim is lost and a concurrent
//! redelivery could dispatch twice — i.e. it degrades back to the pre-existing
//! at-least-once behaviour. We accept that floor deliberately: the dispatched jobs
//! carry no natural idempotence key (each `run_automation`/`run_profile` enqueue
//! gets a fresh id), but a duplicate is a redundant agent reply, never corruption
//! — not worth a durable table. A transient bus error on the claim likewise
//! **fails open** (dispatch anyway), so a bus blip never silently drops a message.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use catalerum_automation::TriggerEvent;
use catalerum_bus::{Bus, DistLock, LockGuard};
use catalerum_channels::{Channel, InMessage};
use catalerum_core::WorkspaceId;
use catalerum_store::Store;

/// How long to wait before re-subscribing after a stream ends or errors, so a
/// down/misconfigured homeserver or bot doesn't reconnect in a tight loop.
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// Leader-lease TTL for a channel's singleton consumer (SOUL §6.6/§16 M7). One
/// pod per `(workspace, channel)` holds this lease and is the *only* consumer, so
/// two pods never both drain a Matrix `/sync` or Telegram `getUpdates` and
/// double-dispatch. Generously above the long-poll cycle so a live leader keeps
/// it across a poll; short enough that a crashed leader's channel is retaken
/// within ~one TTL.
const LEADER_TTL: Duration = Duration::from_secs(45);

/// How often the active leader refreshes its lease while consuming — well under
/// [`LEADER_TTL`], so a couple of transient refresh blips don't cost leadership.
const LEADER_RENEW: Duration = Duration::from_secs(15);

/// How long a non-leader waits before re-contending for a channel (the holder may
/// have crashed, freeing the lease).
const LEADER_RETRY: Duration = Duration::from_secs(10);

/// TTL of the per-message dispatch-dedup claim (see module docs). Chosen well
/// above the leader-lease fencing window ([`LEADER_TTL`] = 45 s, plus reconnect
/// slack) so that if a stalled leader and its successor both drain the same
/// message, the first to dispatch holds the claim long enough that the other is
/// guaranteed to see it and skip. The claim is **never released** — it simply
/// expires, so it self-cleans and only ever suppresses a *near-simultaneous*
/// redelivery, never a legitimately new message minutes later.
const DISPATCH_CLAIM_TTL: Duration = Duration::from_secs(600);

/// The bus [`DistLock`] resource name for singleton leadership of one channel in
/// one workspace: `leader:channel:{workspace}:{name}`. The same `(workspace,
/// channel)` yields the same key on every pod, so exactly one pod wins the lease
/// and becomes the channel's sole consumer.
pub(crate) fn channel_leader_key(workspace: WorkspaceId, name: &str) -> String {
    format!("leader:channel:{workspace}:{name}")
}

/// The bus [`DistLock`] resource name that claims one inbound message's provider
/// identity for exactly-once dispatch (see the module "dispatch dedup" docs):
/// `channel-msg:{workspace}:{channel}:{source}:{message_id}`. Two pods overlapping
/// in the lease fencing window derive the *same* key for the same message, so only
/// one wins the claim and dispatches. `source` (the room/chat id) is part of the
/// identity because a Telegram `message_id` is unique only within a chat — a bare
/// `{channel}:{message_id}` would falsely dedup two different chats' msg #7.
/// The pieces are opaque here (the key is never parsed back), and each provider's
/// `(source, message_id)` is internally unambiguous (Matrix `event_id` is globally
/// unique on its own; Telegram's ids are colon-free numerics), so no realistic
/// two distinct messages collide onto one key.
pub(crate) fn channel_msg_claim_key(
    workspace: WorkspaceId,
    channel: &str,
    source: &str,
    message_id: &str,
) -> String {
    format!("channel-msg:{workspace}:{channel}:{source}:{message_id}")
}

/// Whether [`dispatch_inbound`] should proceed with dispatch or skip it as a
/// duplicate a peer already handled.
enum DispatchClaim {
    /// Dispatch: we won the claim, the message has no id (at-least-once), or the
    /// bus errored and we fail open rather than drop mail.
    Proceed,
    /// Skip silently: a peer in the fencing window already claimed + dispatched
    /// this exact message identity.
    Skip,
}

/// Claim `msg`'s provider identity so that, across the leader-lease fencing window
/// (module docs), exactly one pod dispatches it. A bus [`DistLock::try_acquire`] on
/// [`channel_msg_claim_key`] with [`DISPATCH_CLAIM_TTL`]: the winner gets
/// [`DispatchClaim::Proceed`] (the guard is intentionally dropped un-released — the
/// claim expires by TTL, never freed); a loser gets [`DispatchClaim::Skip`].
///
/// - A message with **no `message_id`** claims nothing and always proceeds
///   (at-least-once — never drop mail for want of an id).
/// - A **bus error** fails open (proceeds) — a transient Valkey blip degrades to
///   the pre-existing at-least-once behaviour, it never silently drops a message.
async fn claim_dispatch(
    lock: &dyn DistLock,
    workspace_id: WorkspaceId,
    channel: &str,
    msg: &InMessage,
) -> DispatchClaim {
    let Some(message_id) = msg.message_id.as_deref() else {
        return DispatchClaim::Proceed; // no provider id → at-least-once
    };
    let key = channel_msg_claim_key(workspace_id, channel, &msg.source, message_id);
    match lock.try_acquire(&key, DISPATCH_CLAIM_TTL).await {
        // Won the claim: dispatch. Drop the guard un-released so it TTL-expires.
        Ok(Some(_guard)) => DispatchClaim::Proceed,
        // A peer already dispatched this message identity within the window.
        Ok(None) => DispatchClaim::Skip,
        Err(e) => {
            // Fail open: a bus error must never cost us a message (at-least-once).
            warn!(channel = %channel, error = %e, "dispatch dedup claim failed; dispatching anyway (at-least-once)");
            DispatchClaim::Proceed
        }
    }
}

/// Subscribes to inbound-capable channels and turns each received message into a
/// `ChannelMessage` trigger dispatch (SOUL §11/§25) **and** a channel→profile
/// dispatch (SOUL §19): any [`AgentProfile`](catalerum_core::model::AgentProfile)
/// listening on the channel runs the §7 loop and replies in place.
pub struct ChannelListener {
    store: Store,
    workspace_id: WorkspaceId,
    channels: HashMap<String, Arc<dyn Channel>>,
    /// The coordination bus — its [`DistLock`] backs the per-channel leader lease
    /// that keeps a channel to one consumer across pods (SOUL §6.6/§16 M7).
    bus: Bus,
}

impl ChannelListener {
    /// A listener that dispatches messages from `channels` into `workspace_id` —
    /// both to matching automations and (durably, via the job queue) to agent
    /// profiles listening on the channel. `bus` gates each channel's listen loop
    /// with a leader lease so, across pods, only one consumes it.
    #[must_use]
    pub fn new(
        store: Store,
        workspace_id: WorkspaceId,
        channels: HashMap<String, Arc<dyn Channel>>,
        bus: Bus,
    ) -> Self {
        Self {
            store,
            workspace_id,
            channels,
            bus,
        }
    }

    /// Spawn the listener: one detached task per channel, each (re)subscribing
    /// forever. Returns the supervising [`JoinHandle`]. A no-op when no channels
    /// are inbound-enabled (the returned task completes immediately).
    #[must_use]
    pub fn spawn(self) -> JoinHandle<()> {
        tokio::spawn(self.run())
    }

    /// Spawn a per-channel listen loop and hold the handles (the loops run for the
    /// process lifetime).
    async fn run(self) {
        let Self {
            store,
            workspace_id,
            channels,
            bus,
        } = self;
        let mut tasks = Vec::with_capacity(channels.len());
        for (name, channel) in channels {
            tasks.push(tokio::spawn(listen_channel(
                store.clone(),
                workspace_id,
                name,
                channel,
                bus.clone(),
            )));
        }
        for t in tasks {
            let _ = t.await;
        }
    }
}

/// Contend-then-consume loop for one channel (SOUL §6.6/§16 M7): try to acquire
/// this `(workspace, channel)`'s leader lease; only the winner subscribes and
/// dispatches (see [`drain_while_leader`]). A loser waits [`LEADER_RETRY`] and
/// re-contends (the leader may crash). On the in-process bus the lease is
/// uncontended, so single-pod behaviour is unchanged.
async fn listen_channel(
    store: Store,
    workspace_id: WorkspaceId,
    name: String,
    channel: Arc<dyn Channel>,
    bus: Bus,
) {
    let key = channel_leader_key(workspace_id, &name);
    loop {
        match bus.lock().try_acquire(&key, LEADER_TTL).await {
            Ok(Some(guard)) => {
                info!(channel = %name, kind = channel.kind(), "acquired channel leadership; consuming");
                drain_while_leader(&store, workspace_id, &name, &channel, bus.lock(), &guard).await;
                // Hand leadership back so a peer can take over promptly (best-effort;
                // a no-op if we already lost the lease to expiry).
                let _ = bus.lock().release(&guard).await;
            }
            Ok(None) => {
                // Another pod leads this channel; wait, then re-contend.
                tokio::time::sleep(LEADER_RETRY).await;
            }
            Err(e) => {
                warn!(channel = %name, error = %e, "channel leader-lock error; retrying");
                tokio::time::sleep(LEADER_RETRY).await;
            }
        }
    }
}

/// While holding this channel's leader lease, subscribe and drain the inbound
/// stream, dispatching each message and periodically refreshing the lease.
/// Returns when the stream ends/errors (→ caller releases + re-contends) or when
/// the lease is definitively lost (→ stop consuming; the new leader owns it).
async fn drain_while_leader(
    store: &Store,
    workspace_id: WorkspaceId,
    name: &str,
    channel: &Arc<dyn Channel>,
    lock: &dyn DistLock,
    guard: &LockGuard,
) {
    match channel.subscribe().await {
        Ok(mut stream) => {
            info!(channel = %name, kind = channel.kind(), "channel listener subscribed");
            let mut renew = tokio::time::interval(LEADER_RENEW);
            loop {
                tokio::select! {
                    item = stream.next() => match item {
                        Some(Ok(msg)) => dispatch_inbound(store, workspace_id, name, msg, lock).await,
                        Some(Err(e)) => {
                            warn!(channel = %name, error = %e, "inbound stream error; resubscribing");
                            break;
                        }
                        None => break, // stream ended → caller re-contends + re-subscribes
                    },
                    _ = renew.tick() => {
                        match lock.refresh(guard, LEADER_TTL).await {
                            Ok(true) => {}
                            Ok(false) => {
                                // Lease expired and was taken by a peer: stop mutating
                                // immediately so we don't double-dispatch alongside the
                                // new leader (the fencing caveat in the module docs).
                                warn!(channel = %name, "lost channel leadership; stopping consume");
                                return;
                            }
                            Err(e) => {
                                // Transient bus error — the lease still has TTL slack,
                                // so keep consuming and retry the refresh next tick.
                                warn!(channel = %name, error = %e, "channel lease refresh error; will retry");
                            }
                        }
                    }
                }
            }
        }
        Err(e) => {
            warn!(channel = %name, error = %e, "channel subscribe failed; retrying");
        }
    }
    // Pause before the caller re-contends so a down homeserver/bot doesn't spin.
    tokio::time::sleep(RECONNECT_DELAY).await;
}

/// Dispatch one inbound message as a `ChannelMessage` trigger (SOUL §11) **and**
/// to any agent profiles listening on the channel (SOUL §19/§25). The channel
/// `name` is decisive for matching (and routes the agent's auto-reply back to the
/// same room); `sender` rides along so a group-chat agent knows who spoke.
/// Best-effort: a dispatch failure is logged, never fatal to the listen loop.
///
/// Before dispatching, [`claim_dispatch`] claims the message's provider identity
/// on `lock` so that, if the leader lease's fencing window let a second pod drain
/// the same message, only one pod actually dispatches (exactly-once; module docs).
async fn dispatch_inbound(
    store: &Store,
    workspace_id: WorkspaceId,
    name: &str,
    msg: InMessage,
    lock: &dyn DistLock,
) {
    // Exactly-once claim across the lease fencing window (module docs). Gate BOTH
    // dispatch paths below — one message → at most one claim → at most one set of
    // jobs, even if a stalled leader and its successor both saw this message.
    if let DispatchClaim::Skip = claim_dispatch(lock, workspace_id, name, &msg).await {
        debug!(
            channel = %name,
            message_id = ?msg.message_id,
            "inbound message already dispatched by a peer; skipping (exactly-once dedup)"
        );
        return;
    }
    // Channel→profile routing (SOUL §19/§25): enqueue a durable `run_profile` job
    // per profile listening here — each runs its own scoped loop on the worker and
    // replies in place. Independent of the automation path; best-effort (logged).
    match catalerum_ingest::dispatch_channel_to_profiles(store, workspace_id, name, &msg.text).await
    {
        Ok(jobs) if !jobs.is_empty() => {
            info!(channel = %name, profiles = jobs.len(), "inbound channel message → agent profiles")
        }
        Ok(_) => {}
        Err(e) => warn!(channel = %name, error = %e, "dispatching channel profiles failed"),
    }
    let event = TriggerEvent::ChannelMessage {
        channel: name.to_string(),
        text: Some(msg.text),
        sender: (!msg.sender.is_empty()).then_some(msg.sender),
    };
    match catalerum_ingest::dispatch_trigger_event(store, workspace_id, &event).await {
        Ok(jobs) if !jobs.is_empty() => {
            info!(channel = %name, matched = jobs.len(), "inbound channel message dispatched")
        }
        Ok(_) => {} // no automation matched — nothing to run
        Err(e) => {
            warn!(channel = %name, error = %e, "dispatching inbound channel message failed")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        channel_leader_key, channel_msg_claim_key, claim_dispatch, DispatchClaim, LEADER_TTL,
    };
    use catalerum_bus::Bus;
    use catalerum_channels::InMessage;
    use catalerum_core::WorkspaceId;

    /// Build an [`InMessage`] with the given provider id (or `None`) for the
    /// dedup-claim tests; sender/text are immaterial to the claim.
    fn in_msg(source: &str, message_id: Option<&str>) -> InMessage {
        InMessage {
            sender: "@someone:hs".to_string(),
            text: "hello".to_string(),
            source: source.to_string(),
            message_id: message_id.map(str::to_string),
        }
    }

    #[test]
    fn channel_leader_key_is_per_workspace_and_channel() {
        let ws1 = WorkspaceId::new();
        let ws2 = WorkspaceId::new();
        // A different channel in the same workspace → a different lease (they lead
        // independently), and the same channel in a different workspace too.
        assert_ne!(
            channel_leader_key(ws1, "matrix"),
            channel_leader_key(ws1, "telegram")
        );
        assert_ne!(
            channel_leader_key(ws1, "matrix"),
            channel_leader_key(ws2, "matrix")
        );
        // The same (workspace, channel) is stable across pods (they collide → one leader).
        assert_eq!(
            channel_leader_key(ws1, "matrix"),
            channel_leader_key(ws1, "matrix")
        );
        assert!(channel_leader_key(ws1, "matrix").starts_with("leader:channel:"));
    }

    /// The property the singleton-consumer fix rests on: for one `(workspace,
    /// channel)`, exactly one pod can hold the leader lease at a time, the holder
    /// can renew it while consuming, and once it releases (or crashes → the lease
    /// TTL-expires) a peer can take over. Exercised against the in-process bus
    /// (same primitive the Valkey backend implements, whose fencing is proved in
    /// `catalerum-bus`).
    #[tokio::test]
    async fn only_one_pod_leads_a_channel() {
        let bus = Bus::in_process();
        let ws = WorkspaceId::new();
        let key = channel_leader_key(ws, "matrix");

        // Pod A wins leadership and becomes the channel's sole consumer.
        let a = bus.lock().try_acquire(&key, LEADER_TTL).await.unwrap();
        assert!(a.is_some(), "the first pod becomes the channel's leader");
        let guard = a.unwrap();

        // Pod B contends for the SAME channel and is refused → it must not consume.
        assert!(
            bus.lock()
                .try_acquire(&key, LEADER_TTL)
                .await
                .unwrap()
                .is_none(),
            "a second pod must NOT also consume the channel"
        );

        // The leader renews its lease while draining the stream.
        assert!(bus.lock().refresh(&guard, LEADER_TTL).await.unwrap());

        // On graceful exit it releases; a peer can then take over.
        assert!(bus.lock().release(&guard).await.unwrap());
        assert!(
            bus.lock()
                .try_acquire(&key, LEADER_TTL)
                .await
                .unwrap()
                .is_some(),
            "after the leader releases, a peer takes over"
        );
    }

    /// The dedup claim key is stable for one message identity but distinct across
    /// workspace, channel, **source** (Telegram's per-chat `message_id`), and
    /// message id — the shape the exactly-once dispatch rests on.
    #[test]
    fn channel_msg_claim_key_identifies_one_message() {
        let ws = WorkspaceId::new();
        // Same (ws, channel, source, id) → same key: two pods collide → one claims.
        assert_eq!(
            channel_msg_claim_key(ws, "telegram", "42", "7"),
            channel_msg_claim_key(ws, "telegram", "42", "7"),
        );
        // Same message id in a DIFFERENT chat must NOT collide — a Telegram
        // `message_id` is unique only within a chat, so folding in `source` stops
        // chat 99's msg #7 being dropped as a "duplicate" of chat 42's msg #7.
        assert_ne!(
            channel_msg_claim_key(ws, "telegram", "42", "7"),
            channel_msg_claim_key(ws, "telegram", "99", "7"),
        );
        // Different message id in the same chat differs.
        assert_ne!(
            channel_msg_claim_key(ws, "telegram", "42", "7"),
            channel_msg_claim_key(ws, "telegram", "42", "8"),
        );
        // Same id, different channel differs (channels lead + dedup independently).
        assert_ne!(
            channel_msg_claim_key(ws, "matrix", "!r:hs", "$e"),
            channel_msg_claim_key(ws, "telegram", "!r:hs", "$e"),
        );
        // Same id, different workspace differs (tenant isolation).
        let ws2 = WorkspaceId::new();
        assert_ne!(
            channel_msg_claim_key(ws, "matrix", "!r:hs", "$e"),
            channel_msg_claim_key(ws2, "matrix", "!r:hs", "$e"),
        );
        assert!(channel_msg_claim_key(ws, "matrix", "!r:hs", "$e").starts_with("channel-msg:"));
    }

    /// The core exactly-once property: when the lease fencing window lets two pods
    /// both drain the *same* message, only the first to claim it dispatches; the
    /// second sees the claim taken and skips → one delivery. Exercised against the
    /// in-process bus (the same [`catalerum_bus::DistLock`] the Valkey backend
    /// implements), simulating the double-dispatch as two `claim_dispatch` calls
    /// on a shared bus.
    #[tokio::test]
    async fn a_message_is_dispatched_once_across_overlapping_pods() {
        let bus = Bus::in_process();
        let ws = WorkspaceId::new();
        let msg = in_msg("!room:hs", Some("$evt1"));

        // Pod A drains + dispatches first → wins the claim → proceeds.
        assert!(
            matches!(
                claim_dispatch(bus.lock(), ws, "matrix", &msg).await,
                DispatchClaim::Proceed
            ),
            "the first pod to see the message dispatches it"
        );
        // Pod B (stalled-leader overlap) drains the SAME message → claim refused →
        // skips, so the message is delivered exactly once.
        assert!(
            matches!(
                claim_dispatch(bus.lock(), ws, "matrix", &msg).await,
                DispatchClaim::Skip
            ),
            "a second pod in the fencing window must NOT re-dispatch the same message"
        );
        // A genuinely different message id is unaffected — dedup is per-message.
        let other = in_msg("!room:hs", Some("$evt2"));
        assert!(
            matches!(
                claim_dispatch(bus.lock(), ws, "matrix", &other).await,
                DispatchClaim::Proceed
            ),
            "a different message still dispatches"
        );
    }

    /// A message with **no `message_id`** (a provider that omits it) claims nothing
    /// and always dispatches — at-least-once is preserved, we never drop mail for
    /// want of an id, even if the identical message arrives twice.
    #[tokio::test]
    async fn a_message_without_an_id_always_dispatches() {
        let bus = Bus::in_process();
        let ws = WorkspaceId::new();
        let msg = in_msg("!room:hs", None);
        for _ in 0..2 {
            assert!(
                matches!(
                    claim_dispatch(bus.lock(), ws, "matrix", &msg).await,
                    DispatchClaim::Proceed
                ),
                "an id-less message dispatches unconditionally (at-least-once)"
            );
        }
    }
}
