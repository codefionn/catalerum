use super::*;

/// Await roughly `ms` milliseconds (a wasm-friendly sleep for reconnect backoff).
pub(super) async fn sleep_ms(ms: u32) {
    let (tx, rx) = futures::channel::oneshot::channel::<()>();
    gloo_timers::callback::Timeout::new(ms, move || {
        let _ = tx.send(());
    })
    .forget();
    let _ = rx.await;
}

fn browser_online() -> bool {
    web_sys::window().is_none_or(|w| w.navigator().on_line())
}

pub(super) async fn reconnect_pause(delay_ms: u32) {
    while !browser_online() {
        sleep_ms(1_000).await;
    }
    sleep_ms(delay_ms).await;
}

/// Send a turn-opening [`Outbound`] frame on `sock`. Factored so the initial
/// send can retry once on a fresh connection: a held-open socket that idled
/// through a network drop / proxy timeout looks open until the first send
/// fails, and the failed frame never reached the server, so resending opens
/// no duplicate turn.
pub(super) async fn send_outbound(sock: &mut ChatSocket, outbound: &Outbound) -> Result<(), ChatSocketError> {
    match outbound {
        Outbound::Chat(msg) => sock.send(msg).await,
        Outbound::Approval {
            approval_id,
            approved,
            conversation_id,
            user_message_id,
        } => {
            sock.send_approval(approval_id, *approved, conversation_id, user_message_id)
                .await
        }
        Outbound::Attach {
            conversation_id,
            user_message_id,
            resume_after,
        } => {
            sock.send_attach(conversation_id, user_message_id, resume_after.as_deref())
                .await
        }
    }
}

/// Reconnect the chat socket and re-send a turn-opening frame that (almost
/// certainly) never reached the server: the socket died before any server ack
/// arrived AND the caller's active-turn probe found nothing running, so
/// re-sending opens the turn rather than duplicating it. (The residual race —
/// the frame WAS delivered and its turn already finished before the probe —
/// would duplicate the message; that window is one whole turn completing
/// against a same-instant connection drop.) Bounded backoff like
/// [`reconnect_and_attach`].
pub(super) async fn reconnect_and_resend(token: Option<&str>, outbound: &Outbound) -> Option<ChatSocket> {
    let mut delay_ms = 400u32;
    for _ in 0..8 {
        reconnect_pause(delay_ms).await;
        if let Ok(mut sock) = ChatSocket::connect(token) {
            if send_outbound(&mut sock, outbound).await.is_ok() {
                return Some(sock);
            }
        }
        delay_ms = (delay_ms * 2).min(10_000);
    }
    None
}

/// Reconnect the chat socket and reattach to an in-flight turn's live stream,
/// resuming from `resume_after` with bounded backoff (SOUL §7/§12). Returns the
/// reattached socket, or `None` if every attempt failed (the caller then surfaces
/// the drop). The run keeps executing server-side regardless, so a later reopen
/// still shows the completed turn.
pub(super) async fn reconnect_and_attach(
    token: Option<&str>,
    conversation_id: &str,
    user_message_id: &str,
    resume_after: Option<String>,
) -> Option<ChatSocket> {
    let mut delay_ms = 400u32;
    for _ in 0..8 {
        reconnect_pause(delay_ms).await;
        if let Ok(mut sock) = ChatSocket::connect(token) {
            if sock
                .send_attach(conversation_id, user_message_id, resume_after.as_deref())
                .await
                .is_ok()
            {
                return Some(sock);
            }
        }
        delay_ms = (delay_ms * 2).min(10_000);
    }
    None
}

/// The inputs to one streamed turn over the chat socket, handed to the shared
/// `drive_turn` driver. Both the input sender ([`send_turn`]) and the per-message
/// regenerate build one and call `drive_turn.run(..)` with it.
pub(super) struct DriveTurnArgs {
    /// The frame to send to open the turn.
    pub(super) outbound: Outbound,
    /// The user line whose server message id is backfilled when the turn
    /// completes, so its regenerate control can target it.
    pub(super) user_line_id: usize,
    /// The (empty, streaming) assistant line to fold the first round into.
    pub(super) assistant_id: usize,
}
