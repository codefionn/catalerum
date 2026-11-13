use super::*;

/// A kept-alive `MediaRecorder.ondataavailable` handler (parked so it outlives the
/// recording that installed it).
pub(super) type BlobEventClosure = Closure<dyn FnMut(web_sys::BlobEvent)>;
/// A kept-alive `MediaRecorder.onstop` handler.
pub(super) type StopClosure = Closure<dyn FnMut()>;

/// Stop every track of `stream`, releasing the microphone (and the browser's
/// recording indicator). Used both on the mic setup-failure paths and by
/// `release_media` once a take has been handed off.
pub(super) fn stop_stream_tracks(stream: &web_sys::MediaStream) {
    let tracks = stream.get_tracks();
    for i in 0..tracks.length() {
        if let Ok(track) = tracks.get(i).dyn_into::<web_sys::MediaStreamTrack>() {
            track.stop();
        }
    }
}

/// Arm a voice-activity auto-stop for the live recording (SOUL §7): analyse the
/// mic level every ~120 ms and, once speech has been heard, end the take after
/// ~2.5 s of near-silence — so the composer "records as long as something is
/// happening". A hard cap bounds a stuck stream. Best-effort: if the
/// `AudioContext`/analyser can't be built the manual stop still works, and the
/// speech-first guard means a silent/suspended analyser degrades to the cap rather
/// than cutting a take short. The `AudioContext` + `Interval` are parked in the
/// caller's cells so `release_media` tears them down on finish.
pub(super) fn spawn_vad(
    stream: &web_sys::MediaStream,
    audio_ctx_slot: &Rc<RefCell<Option<web_sys::AudioContext>>>,
    vad_slot: &Rc<RefCell<Option<Interval>>>,
    stop_now: Rc<dyn Fn()>,
    // Voice-overlay taps (both `None` for composer dictation): the live 0..1
    // mic level for the sound-reactive orb, and a "speech was actually heard"
    // flag so an all-silence take (the 60 s cap firing) skips transcription.
    level_out: Option<RwSignal<f32>>,
    heard_out: Option<Rc<Cell<bool>>>,
) {
    let Ok(ctx) = web_sys::AudioContext::new() else {
        return;
    };
    let Ok(source) = ctx.create_media_stream_source(stream) else {
        let _ = ctx.close();
        return;
    };
    let Ok(analyser) = ctx.create_analyser() else {
        let _ = ctx.close();
        return;
    };
    analyser.set_fft_size(1024);
    if source.connect_with_audio_node(&analyser).is_err() {
        let _ = ctx.close();
        return;
    }
    let bins = analyser.frequency_bin_count() as usize;

    // Poll cadence + thresholds. Silence is judged on the waveform's RMS deviation
    // from the 128 zero-line (normalised to [0, 1]); ordinary speech clears it.
    const TICK_MS: u32 = 120;
    const SILENCE_STOP_TICKS: u32 = 21; // ~2.5 s of quiet ends the take
    const MAX_TICKS: u32 = 500; // ~60 s hard cap
    const SPEECH_RMS: f64 = 0.02;

    // `nodes.0` (the source) stays owned so the graph edge isn't GC'd mid-take;
    // `nodes.1` (the analyser) is tapped each tick.
    let nodes = (source, analyser);
    let mut heard_speech = false;
    let mut silent_ticks = 0u32;
    let mut elapsed_ticks = 0u32;
    let mut prev_level = 0f32;
    let interval = Interval::new(TICK_MS, move || {
        elapsed_ticks += 1;
        let mut buf = vec![0u8; bins];
        nodes.1.get_byte_time_domain_data(&mut buf);
        let mut sum = 0f64;
        for &s in &buf {
            let v = (f64::from(s) - 128.0) / 128.0;
            sum += v * v;
        }
        let rms = (sum / buf.len().max(1) as f64).sqrt();
        if let Some(level) = level_out {
            prev_level = voice::level_from_rms(rms, prev_level);
            level.set(prev_level);
        }
        if rms >= SPEECH_RMS {
            heard_speech = true;
            silent_ticks = 0;
            if let Some(heard) = &heard_out {
                heard.set(true);
            }
        } else if heard_speech {
            silent_ticks += 1;
        }
        if (heard_speech && silent_ticks >= SILENCE_STOP_TICKS) || elapsed_ticks >= MAX_TICKS {
            (*stop_now)();
        }
    });
    *audio_ctx_slot.borrow_mut() = Some(ctx);
    *vad_slot.borrow_mut() = Some(interval);
}

/// How many fresh-connection attempts one paragraph's synthesis gets before
/// its error surfaces on the overlay. Synthesis is stateless server-side, so
/// re-speaking the same text on a new socket is always safe.
const SPEECH_FETCH_ATTEMPTS: u32 = 3;
/// The longest silence tolerated between `/ws/speech` events before the
/// socket is presumed half-open (a dropped network keeps a browser WS "open"
/// for minutes with no close event) and the attempt retries on a fresh one.
const SPEECH_EVENT_TIMEOUT_MS: u32 = 20_000;

/// How one synthesis attempt over an open speech socket ended.
enum SpeechAttempt {
    /// The reply's audio, complete.
    Done(Vec<u8>),
    /// The server answered an error frame for this request — a synthesis
    /// failure (no TTS model, bad input), not a transport problem: retrying
    /// the same text would fail the same way.
    Refused(String),
    /// The socket closed or went silent mid-reply; worth a fresh connection.
    Dropped(String),
    /// A newer request took over the channel; abandon quietly.
    Superseded,
}

/// Collect one speak request's reply off the socket: binary chunks bracketed
/// by `speech_start`/`speech_end`, keyed to `req` (frames tagged with another
/// id — an abandoned earlier request — are skipped, never returned). Every
/// wait is capped by [`SPEECH_EVENT_TIMEOUT_MS`] so a half-open socket reads
/// as [`SpeechAttempt::Dropped`] instead of hanging the voice loop.
async fn collect_speech_reply(
    sock: &mut SpeechSocket,
    req: u64,
    speech_ids: &SpeechReqId,
) -> SpeechAttempt {
    let mut bytes = Vec::new();
    let mut collecting = false;
    loop {
        let ev = {
            let next = sock.next_event();
            futures::pin_mut!(next);
            let mut deadline = Box::pin(sleep_ms(SPEECH_EVENT_TIMEOUT_MS).fuse());
            futures::select! {
                ev = next.fuse() => Some(ev),
                () = deadline => None,
            }
        };
        match ev {
            Some(Some(SpeechEvent::Start { id, .. })) => {
                collecting = id == Some(req);
                if collecting {
                    bytes.clear();
                }
            }
            Some(Some(SpeechEvent::Chunk(chunk))) => {
                if collecting {
                    bytes.extend_from_slice(&chunk);
                }
            }
            Some(Some(SpeechEvent::End { id })) => {
                if id == Some(req) {
                    return SpeechAttempt::Done(bytes);
                }
                collecting = false;
            }
            Some(Some(SpeechEvent::Error { id, message })) => {
                if id == Some(req) || id.is_none() {
                    return SpeechAttempt::Refused(message);
                }
            }
            Some(None) => return SpeechAttempt::Dropped("the speech channel closed".to_string()),
            None => return SpeechAttempt::Dropped("speech synthesis timed out".to_string()),
        }
        if speech_ids.current() != req {
            return SpeechAttempt::Superseded;
        }
    }
}

/// Whether a REST failure is worth retrying: a transport error (network
/// drop, offline, CORS-refused during a server restart) or a transient
/// server status. A definitive 4xx (no STT model configured, bad audio)
/// would fail identically, so it surfaces at once.
pub(super) fn transient_rest_error(e: &rest::RestError) -> bool {
    match e {
        rest::RestError::Transport(_) => true,
        rest::RestError::Status { status, .. } => *status == 429 || *status >= 500,
        rest::RestError::Encode(_) | rest::RestError::Decode(_) => false,
    }
}

/// Linearly resample and down-mix decoded microphone PCM so its wall-clock
/// duration is divided by `speed`. Keeping the original sample rate means the
/// resulting WAV really is shorter to a duration-billed STT provider (changing a
/// playback-rate flag would not affect the uploaded recording).
pub(super) fn compress_pcm(channels: &[Vec<f32>], speed: f32) -> Vec<f32> {
    let input_len = channels.iter().map(Vec::len).min().unwrap_or(0);
    if input_len == 0 || channels.is_empty() {
        return Vec::new();
    }
    let speed = speed.clamp(1.0, 2.0);
    let output_len = ((input_len as f64) / f64::from(speed)).ceil() as usize;
    let mut output = Vec::with_capacity(output_len);
    for frame in 0..output_len {
        let source = (frame as f64 * f64::from(speed)).min((input_len - 1) as f64);
        let left = source.floor() as usize;
        let right = (left + 1).min(input_len - 1);
        let fraction = (source - left as f64) as f32;
        let sample = channels
            .iter()
            .map(|channel| channel[left] + (channel[right] - channel[left]) * fraction)
            .sum::<f32>()
            / channels.len() as f32;
        output.push(sample.clamp(-1.0, 1.0));
    }
    output
}

/// Encode mono floating-point PCM as a provider-friendly 16-bit WAV.
pub(super) fn pcm16_wav(samples: &[f32], sample_rate: u32) -> Result<Vec<u8>, String> {
    let data_len = samples
        .len()
        .checked_mul(2)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| "the compressed recording is too large".to_string())?;
    let riff_len = 36u32
        .checked_add(data_len)
        .ok_or_else(|| "the compressed recording is too large".to_string())?;
    let byte_rate = sample_rate
        .checked_mul(2)
        .ok_or_else(|| "invalid recording sample rate".to_string())?;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&riff_len.to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk length
    wav.extend_from_slice(&1u16.to_le_bytes()); // integer PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for &sample in samples {
        let pcm = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16;
        wav.extend_from_slice(&pcm.to_le_bytes());
    }
    Ok(wav)
}

/// Decode the browser's MediaRecorder container, time-compress it, down-mix to
/// mono, and return a self-describing WAV for `/audio/transcriptions`.
pub(super) async fn compress_recording(bytes: &[u8], speed: f32) -> Result<Vec<u8>, String> {
    let ctx = web_sys::AudioContext::new()
        .map_err(|_| "could not initialize recording compression".to_string())?;
    let array = js_sys::Uint8Array::from(bytes).buffer();
    let decoded = match ctx.decode_audio_data(&array) {
        Ok(promise) => JsFuture::from(promise).await,
        Err(error) => Err(error),
    };
    let _ = ctx.close();
    let buffer: web_sys::AudioBuffer = decoded
        .map_err(|_| "could not decode the microphone recording".to_string())?
        .dyn_into()
        .map_err(|_| "recording decode returned no audio".to_string())?;
    let mut channels = Vec::with_capacity(buffer.number_of_channels() as usize);
    for channel in 0..buffer.number_of_channels() {
        channels.push(
            buffer
                .get_channel_data(channel)
                .map_err(|_| "could not read decoded microphone audio".to_string())?
                .to_vec(),
        );
    }
    let samples = compress_pcm(&channels, speed);
    if samples.is_empty() {
        return Err("the decoded microphone recording was empty".to_string());
    }
    pcm16_wav(&samples, buffer.sample_rate().round() as u32)
}

/// POST a recorded take for transcription, retrying transient failures with
/// a short backoff. The recording only exists client-side — if the POST is
/// given up, the utterance is gone and the user has to say it again — so a
/// connection blip is worth two more tries before that.
pub(super) async fn transcribe_with_retry(
    token: Option<&str>,
    request_id: &str,
    bytes: &[u8],
    content_type: Option<&str>,
) -> Result<rest::Transcription, rest::RestError> {
    let mut delay = 400u32;
    let mut upload_again = true;
    for _ in 0..8 {
        let payload = if upload_again { bytes } else { &[] };
        match rest::transcribe_audio(token, request_id, payload, content_type).await {
            Err(rest::RestError::Status { status: 404, .. }) if !upload_again => {
                // The short Valkey cache lapsed or the original upload never
                // arrived. We still retain the browser recording, so re-upload it.
                upload_again = true;
                continue;
            }
            Err(e) if transient_rest_error(&e) => {
                upload_again = false;
                reconnect_pause(delay).await;
                delay = (delay * 2).min(10_000);
            }
            other => return other,
        }
    }
    rest::transcribe_audio(token, request_id, bytes, content_type).await
}

pub(super) async fn create_conversation_with_retry(
    token: Option<&str>,
    body: &CreateConversation,
) -> Result<Conversation, rest::RestError> {
    let mut delay = 400u32;
    for _ in 0..8 {
        match rest::create_conversation(token, body).await {
            Err(e) if transient_rest_error(&e) => {
                reconnect_pause(delay).await;
                delay = (delay * 2).min(10_000);
            }
            other => return other,
        }
    }
    rest::create_conversation(token, body).await
}

/// Fetch one spoken paragraph's audio over the held-open `/ws/speech` socket
/// (SOUL §7/§12): take the session socket (or open one), send the speak
/// request, collect its binary chunks by request id, and hand the socket back
/// for the next paragraph. A connection drop — a stale held-open socket, a
/// failed send, a mid-reply close, or an event timeout — retries the SAME
/// paragraph on a fresh socket up to [`SPEECH_FETCH_ATTEMPTS`] times with a
/// short backoff, so a network blip costs a pause, not the rest of the reply.
/// A server-side synthesis error never retries (it would fail identically).
pub(super) async fn fetch_speech_bytes(
    speech_sock: &Rc<RefCell<Option<SpeechSocket>>>,
    speech_ids: &SpeechReqId,
    voice_state: RwSignal<VoiceState>,
    text: &str,
) -> Result<Vec<u8>, String> {
    let req = speech_ids.next();
    let token = auth::resolve_token();
    let mut last_err = "could not reach speech synthesis".to_string();
    for attempt in 0..SPEECH_FETCH_ATTEMPTS {
        if attempt > 0 {
            sleep_ms(400 * attempt).await;
        }
        // Superseded (a newer paragraph/turn) or closed while backing off:
        // abandon quietly — the caller's generation check swallows this error.
        if speech_ids.current() != req || voice_state.get_untracked() == VoiceState::Off {
            return Err("superseded".to_string());
        }
        let mut sock = match speech_sock.borrow_mut().take() {
            Some(s) => s,
            None => match SpeechSocket::connect(token.as_deref()) {
                Ok(s) => s,
                Err(e) => {
                    last_err = format!("speech channel failed: {e}");
                    continue;
                }
            },
        };
        if sock.speak(req, text).await.is_err() {
            // The held-open socket was stale (or the fresh one died at once);
            // drop it and retry on a new connection.
            last_err = "could not reach speech synthesis".to_string();
            continue;
        }
        match collect_speech_reply(&mut sock, req, speech_ids).await {
            SpeechAttempt::Done(bytes) => {
                if voice_state.get_untracked() != VoiceState::Off {
                    *speech_sock.borrow_mut() = Some(sock);
                }
                if bytes.is_empty() {
                    return Err("speech synthesis returned no audio".to_string());
                }
                return Ok(bytes);
            }
            // The socket survives a refused request (the protocol keeps it
            // usable); the failure itself is permanent, so surface it.
            SpeechAttempt::Refused(message) => {
                if voice_state.get_untracked() != VoiceState::Off {
                    *speech_sock.borrow_mut() = Some(sock);
                }
                return Err(message);
            }
            SpeechAttempt::Superseded => return Err("superseded".to_string()),
            SpeechAttempt::Dropped(message) => last_err = message,
        }
    }
    Err(last_err)
}
