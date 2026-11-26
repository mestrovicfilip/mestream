//! WebRTC connection to the console: media tracks plus the four data channels
//! (message, control, input, chat) and their start-up handshake.

use crate::gamepad;
use crate::input::{self, GamepadState, InputEvent};
use crate::video::AccessUnit;
use crate::xhome::{RemoteCandidate, Resolution};
use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use serde_json::{Value, json};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::watch;
use webrtc::api::APIBuilder;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MIME_TYPE_H264, MIME_TYPE_OPUS, MediaEngine};
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp::codecs::h264::H264Packet;
use webrtc::rtp::packetizer::Depacketizer;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::{RTCPFeedback, RTCRtpTransceiverInit};
use webrtc::track::track_remote::TrackRemote;

#[derive(Default)]
pub struct Stats {
    pub video_packets: AtomicU64,
    pub video_bytes: AtomicU64,
    pub video_frames: AtomicU64,
    pub video_keyframes: AtomicU64,
    pub video_lost: AtomicU64,
    /// Longest wait between two consecutive pictures since the last read, in microseconds.
    pub video_max_gap_us: AtomicU64,
    pub audio_packets: AtomicU64,
    pub server_width: AtomicU32,
    pub server_height: AtomicU32,
}

pub struct Stream {
    pc: Arc<RTCPeerConnection>,
    pub stats: Arc<Stats>,
    candidates: Arc<Mutex<Vec<RTCIceCandidate>>>,
    state: watch::Receiver<RTCPeerConnectionState>,
    /// Tells the input task to let go of the shared input queue.
    stop: watch::Sender<bool>,
}

/// Controller and frame-timing events. Shared so a reconnect can pick up
/// where the previous connection left off.
pub type InputQueue = Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<InputEvent>>>;

fn media_engine() -> Result<MediaEngine> {
    let mut m = MediaEngine::default();
    let feedback = |kind: &str, parameter: &str| RTCPFeedback {
        typ: kind.to_owned(),
        parameter: parameter.to_owned(),
    };
    let video_feedback = vec![
        feedback("goog-remb", ""),
        feedback("transport-cc", ""),
        feedback("ccm", "fir"),
        feedback("nack", ""),
        feedback("nack", "pli"),
    ];
    // Main profile first, then constrained baseline, then baseline.
    for (payload_type, profile) in [(102u8, "4d001f"), (106, "42e01f"), (108, "42001f")] {
        m.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_H264.to_owned(),
                    clock_rate: 90000,
                    channels: 0,
                    sdp_fmtp_line: format!(
                        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id={profile}"
                    ),
                    rtcp_feedback: video_feedback.clone(),
                },
                payload_type,
                ..Default::default()
            },
            RTPCodecType::Video,
        )?;
    }
    m.register_codec(
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48000,
                channels: 2,
                sdp_fmtp_line: "minptime=10;useinbandfec=1;stereo=1".to_owned(),
                rtcp_feedback: vec![],
            },
            payload_type: 111,
            ..Default::default()
        },
        RTPCodecType::Audio,
    )?;
    Ok(m)
}

async fn send_json(channel: &RTCDataChannel, value: &Value) {
    // The console expects JSON as binary frames, like the web client sends them.
    if let Err(e) = channel.send(&Bytes::from(value.to_string())).await {
        crate::status!("[{}] send failed: {e}", channel.label());
    }
}

fn message(target: &str, content: Value) -> Value {
    json!({
        "type": "Message",
        "content": content.to_string(),
        "id": uuid::Uuid::new_v4().to_string(),
        "target": target,
        "cv": "",
    })
}

const MAX_ACCESS_UNIT_BYTES: usize = 4 * 1024 * 1024;

/// Where received video goes.
#[derive(Clone, Default)]
pub struct VideoSink {
    /// Append the raw Annex B stream to this file.
    pub dump: Option<PathBuf>,
    /// Deliver complete pictures to the decoder thread.
    pub frames: Option<std::sync::mpsc::Sender<AccessUnit>>,
    /// Deliver Opus packets to the audio thread.
    pub audio: Option<std::sync::mpsc::Sender<Bytes>>,
}

async fn read_video(
    track: Arc<TrackRemote>,
    stats: Arc<Stats>,
    sink: VideoSink,
    control: Arc<RTCDataChannel>,
) {
    let mut out = sink.dump.and_then(|p| match std::fs::File::create(&p) {
        Ok(f) => Some(std::io::BufWriter::new(f)),
        Err(e) => {
            crate::status!("[video] cannot create {}: {e}", p.display());
            None
        }
    });
    let mut depacketizer = H264Packet::default();
    let mut last_seq: Option<u16> = None;
    let mut unit: Vec<u8> = Vec::new();
    let mut unit_ts: Option<u32> = None;
    let mut unit_arrival = Instant::now();
    let mut unit_is_keyframe = false;
    // Nothing can be decoded until a keyframe arrives, at start and after loss.
    let mut waiting_for_keyframe = true;
    let mut last_keyframe_request: Option<Instant> = None;

    while let Ok((packet, _)) = track.read_rtp().await {
        let payload = &packet.payload;
        stats.video_packets.fetch_add(1, Ordering::Relaxed);
        stats.video_bytes.fetch_add(payload.len() as u64, Ordering::Relaxed);

        let seq = packet.header.sequence_number;
        let mut lost = false;
        if let Some(prev) = last_seq {
            let gap = seq.wrapping_sub(prev);
            if gap == 0 || gap >= 0x8000 {
                continue; // duplicate or late retransmission: the picture is already gone
            }
            if gap > 1 {
                stats.video_lost.fetch_add(u64::from(gap - 1), Ordering::Relaxed);
                lost = true;
            }
        }
        last_seq = Some(seq);

        if unit_ts != Some(packet.header.timestamp) {
            if !unit.is_empty() {
                lost = true; // previous picture never got its final packet
            }
            unit_ts = Some(packet.header.timestamp);
            unit.clear();
            unit_is_keyframe = false;
            let now = Instant::now();
            let gap = now.duration_since(unit_arrival).as_micros() as u64;
            stats.video_max_gap_us.fetch_max(gap, Ordering::Relaxed);
            unit_arrival = now;
            stats.video_frames.fetch_add(1, Ordering::Relaxed);
        }

        if lost {
            waiting_for_keyframe = true;
            depacketizer = H264Packet::default();
            unit.clear();
            unit_is_keyframe = false;
        }
        if waiting_for_keyframe
            && last_keyframe_request.is_none_or(|t| t.elapsed() > std::time::Duration::from_millis(500))
        {
            last_keyframe_request = Some(Instant::now());
            send_json(&control, &json!({ "message": "videoKeyframeRequested", "ifrRequested": false })).await;
        }

        if starts_keyframe(payload) {
            stats.video_keyframes.fetch_add(1, Ordering::Relaxed);
            unit_is_keyframe = true;
        }
        if let Ok(annex_b) = depacketizer.depacketize(payload) {
            if let Some(file) = out.as_mut() {
                let _ = file.write_all(&annex_b);
            }
            unit.extend_from_slice(&annex_b);
        }
        // No real picture is this large: something is wrong with the stream,
        // so start over from the next keyframe instead of growing forever.
        if unit.len() > MAX_ACCESS_UNIT_BYTES {
            unit.clear();
            waiting_for_keyframe = true;
        }

        if packet.header.marker {
            if unit_is_keyframe {
                waiting_for_keyframe = false;
            }
            if !waiting_for_keyframe
                && !unit.is_empty()
                && let Some(frames) = &sink.frames
            {
                let data = std::mem::take(&mut unit);
                let rtp_timestamp = packet.header.timestamp;
                if frames.send(AccessUnit { data, arrival: unit_arrival, rtp_timestamp }).is_err() {
                    break; // the video window was closed
                }
            }
            unit.clear();
        }
    }
    if let Some(mut file) = out {
        let _ = file.flush();
    }
}

impl Stream {
    pub async fn new(
        resolution: Resolution,
        sink: VideoSink,
        input_events: InputQueue,
    ) -> Result<Self> {
        let (stop, mut stopped) = watch::channel(false);
        let mut engine = media_engine()?;
        let registry = register_default_interceptors(Registry::new(), &mut engine)?;
        let api = APIBuilder::new()
            .with_media_engine(engine)
            .with_interceptor_registry(registry)
            .build();
        let pc = Arc::new(api.new_peer_connection(RTCConfiguration::default()).await?);
        let stats = Arc::new(Stats::default());

        pc.add_transceiver_from_kind(
            RTPCodecType::Audio,
            Some(RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Sendrecv,
                send_encodings: vec![],
            }),
        )
        .await?;
        pc.add_transceiver_from_kind(
            RTPCodecType::Video,
            Some(RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Recvonly,
                send_encodings: vec![],
            }),
        )
        .await?;

        let channel = |protocol: &str| RTCDataChannelInit {
            ordered: Some(true),
            protocol: Some(protocol.to_owned()),
            ..Default::default()
        };
        let _chat = pc.create_data_channel("chat", Some(channel("chatV1"))).await?;
        let control = pc.create_data_channel("control", Some(channel("controlV1"))).await?;
        let input_ch = pc.create_data_channel("input", Some(channel("1.0"))).await?;
        let message_ch = pc.create_data_channel("message", Some(channel("messageV1"))).await?;
        let input_rx = input_ch.clone();
        let control_for_video = control.clone();

        // input: forward controller state and frame timings the moment they
        // happen (the web client batches them to the video frame rate).
        let input_ready = Arc::new(AtomicBool::new(false));
        {
            let ch = input_ch.clone();
            let ready = input_ready.clone();
            tokio::spawn(async move {
                let mut sequence = 0u32;
                let mut pads: Vec<GamepadState> = Vec::new();
                let mut timings = Vec::new();
                // The queue outlives this connection; hold it only while we are alive.
                let mut input_events = input_events.lock().await;
                loop {
                    let first = tokio::select! {
                        event = input_events.recv() => match event {
                            Some(event) => event,
                            None => break,
                        },
                        _ = stopped.changed() => break,
                    };
                    pads.clear();
                    timings.clear();
                    let mut next = Some(first);
                    while let Some(event) = next {
                        match event {
                            InputEvent::Pad(pad) => pads.push(pad),
                            InputEvent::Frame(timing) => timings.push(timing),
                        }
                        next = input_events.try_recv().ok();
                    }
                    if !ready.load(Ordering::Relaxed) {
                        continue;
                    }
                    sequence += 1;
                    let packet = input::report(sequence, input::now_ms(), &timings, &pads);
                    if ch.send(&Bytes::from(packet)).await.is_err() {
                        break;
                    }
                }
            });
        }

        // message: handshake on open, then bring up control + input once acknowledged.
        {
            let ch = message_ch.clone();
            message_ch.on_open(Box::new(move || {
                Box::pin(async move {
                    send_json(
                        &ch,
                        &json!({
                            "type": "Handshake",
                            "version": "messageV1",
                            "id": "be0bfc6d-1e83-4c8a-90ed-fa8601c5a179",
                            "cv": "0",
                        }),
                    )
                    .await;
                })
            }));
        }
        {
            let ch = message_ch.clone();
            let (w, h) = resolution.size();
            message_ch.on_message(Box::new(move |msg: DataChannelMessage| {
                let ch = ch.clone();
                let control = control.clone();
                let input_ch = input_ch.clone();
                let input_ready = input_ready.clone();
                Box::pin(async move {
                    let Ok(data) = serde_json::from_slice::<Value>(&msg.data) else {
                        return;
                    };
                    match data["type"].as_str() {
                        Some("HandshakeAck") => {
                            crate::status!("[message] handshake acknowledged");
                            send_json(
                                &control,
                                &json!({
                                    "message": "authorizationRequest",
                                    "accessKey": "4BDB3609-C1F1-4195-9B37-FEFF45DA8B8E",
                                }),
                            )
                            .await;
                            send_json(
                                &control,
                                &json!({ "message": "gamepadChanged", "gamepadIndex": 0, "wasAdded": true }),
                            )
                            .await;
                            send_json(
                                &control,
                                &json!({ "message": "videoKeyframeRequested", "ifrRequested": false }),
                            )
                            .await;

                            let meta = input::client_metadata(input::now_ms(), 1);
                            match input_ch.send(&Bytes::from(meta)).await {
                                Ok(_) => input_ready.store(true, Ordering::Relaxed),
                                Err(e) => crate::status!("[input] metadata send failed: {e}"),
                            }

                            for m in [
                                message(
                                    "/streaming/systemUi/configuration",
                                    json!({ "version": [0, 2, 0], "systemUis": [] }),
                                ),
                                message(
                                    "/streaming/properties/clientappinstallidchanged",
                                    json!({ "clientAppInstallId": "c97d7ee0-73b2-4239-bf1d-9d805a338429" }),
                                ),
                                message(
                                    "/streaming/characteristics/orientationchanged",
                                    json!({ "orientation": 0 }),
                                ),
                                message(
                                    "/streaming/characteristics/touchinputenabledchanged",
                                    json!({ "touchInputEnabled": false }),
                                ),
                                message("/streaming/characteristics/clientdevicecapabilities", json!({})),
                                message(
                                    "/streaming/characteristics/dimensionschanged",
                                    json!({
                                        "horizontal": w,
                                        "vertical": h,
                                        "preferredWidth": w,
                                        "preferredHeight": h,
                                        "safeAreaLeft": 0,
                                        "safeAreaTop": 0,
                                        "safeAreaRight": w,
                                        "safeAreaBottom": h,
                                        "supportsCustomResolution": true,
                                    }),
                                ),
                            ] {
                                send_json(&ch, &m).await;
                            }
                        }
                        Some("TransactionStart") => {
                            let target = data["target"].as_str().unwrap_or_default();
                            crate::status!("[message] transaction: {target}");
                            if target.ends_with("serverInitiatedDisconnect") {
                                send_json(
                                    &ch,
                                    &json!({
                                        "type": "TransactionComplete",
                                        "content": "\"\"",
                                        "id": data["id"],
                                        "cv": "",
                                    }),
                                )
                                .await;
                            }
                        }
                        Some("Message") => {
                            crate::status!("[message] {}", data["target"].as_str().unwrap_or("?"));
                        }
                        other => crate::status!("[message] unhandled type {other:?}"),
                    }
                })
            }));
        }

        // input: the console reports its encode size (and later, rumble) here.
        {
            let stats = stats.clone();
            input_rx.on_message(Box::new(move |msg: DataChannelMessage| {
                let stats = stats.clone();
                Box::pin(async move {
                    let d = &msg.data;
                    if d.len() >= 10 && u16::from(d[0]) == input::REPORT_SERVER_METADATA {
                        let h = u32::from_le_bytes([d[2], d[3], d[4], d[5]]);
                        let w = u32::from_le_bytes([d[6], d[7], d[8], d[9]]);
                        stats.server_width.store(w, Ordering::Relaxed);
                        stats.server_height.store(h, Ordering::Relaxed);
                        crate::status!("[input] console coordinate space {w}x{h}");
                    } else if d.len() >= 13 && u16::from(d[0]) == input::REPORT_VIBRATION {
                        gamepad::rumble(d[4], d[5], u16::from_le_bytes([d[8], d[9]]));
                    }
                })
            }));
        }

        {
            let stats = stats.clone();
            pc.on_track(Box::new(move |track, _receiver, _transceiver| {
                let stats = stats.clone();
                let sink = sink.clone();
                let control = control_for_video.clone();
                Box::pin(async move {
                    let codec = track.codec();
                    crate::status!(
                        "[track] {} {} ({})",
                        track.kind(),
                        codec.capability.mime_type,
                        codec.capability.sdp_fmtp_line
                    );
                    if track.kind() == RTPCodecType::Video {
                        tokio::spawn(read_video(track, stats, sink, control));
                    } else {
                        tokio::spawn(async move {
                            while let Ok((packet, _)) = track.read_rtp().await {
                                stats.audio_packets.fetch_add(1, Ordering::Relaxed);
                                if let Some(audio) = &sink.audio
                                    && !packet.payload.is_empty()
                                    && audio.send(packet.payload).is_err()
                                {
                                    break;
                                }
                            }
                        });
                    }
                })
            }));
        }

        let candidates = Arc::new(Mutex::new(Vec::new()));
        {
            let candidates = candidates.clone();
            pc.on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
                if let Some(c) = c {
                    candidates.lock().unwrap().push(c);
                }
                Box::pin(async {})
            }));
        }

        let (state_tx, state) = watch::channel(RTCPeerConnectionState::New);
        pc.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
            crate::status!("[webrtc] connection state: {s}");
            let _ = state_tx.send(s);
            Box::pin(async {})
        }));

        Ok(Self { pc, stats, candidates, state, stop })
    }

    /// Creates the offer, starts ICE gathering and returns the SDP to send to
    /// the console (with the requested video bitrate cap applied).
    pub async fn create_offer(&self, video_kbps: u32) -> Result<String> {
        let offer = self.pc.create_offer(None).await?;
        let sdp = set_video_bitrate(&offer.sdp, video_kbps);
        self.pc.set_local_description(offer).await?;
        Ok(sdp)
    }

    pub async fn set_answer(&self, sdp: String) -> Result<()> {
        let answer = RTCSessionDescription::answer(sdp).context("console sent an invalid SDP answer")?;
        self.pc.set_remote_description(answer).await?;
        Ok(())
    }

    /// Waits for ICE gathering to finish and returns our candidates, each as
    /// the JSON string the session API expects.
    pub async fn local_candidates(&self) -> Result<Vec<String>> {
        let mut done = self.pc.gathering_complete_promise().await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), done.recv()).await;
        let list = self.candidates.lock().unwrap().clone();
        let mut out = Vec::with_capacity(list.len());
        for c in list {
            let init = c.to_json()?;
            out.push(
                json!({
                    "candidate": format!("a={}", init.candidate),
                    "sdpMid": init.sdp_mid,
                    "sdpMLineIndex": init.sdp_mline_index,
                    "usernameFragment": init.username_fragment,
                })
                .to_string(),
            );
        }
        if out.is_empty() {
            return Err(anyhow!("no local network candidates found - is the network up?"));
        }
        Ok(out)
    }

    pub async fn add_remote_candidates(&self, remote: Vec<RemoteCandidate>) -> Result<()> {
        let mut added = 0;
        for c in remote {
            let line = c.candidate.trim().trim_start_matches("a=");
            if !line.starts_with("candidate:") {
                continue; // e.g. end-of-candidates
            }
            let mut lines = vec![line.to_owned()];
            // The console often only advertises a Teredo address; the real
            // IPv4 endpoint is encoded inside it.
            if let Some((ip, port)) = line.split(' ').nth(4).and_then(teredo_endpoint) {
                lines.push(format!("candidate:10 1 UDP 1 {ip} 9002 typ host"));
                lines.push(format!("candidate:11 1 UDP 1 {ip} {port} typ host"));
            }
            for candidate in lines {
                let init = RTCIceCandidateInit {
                    candidate: candidate.clone(),
                    sdp_mid: c.sdp_mid.clone(),
                    sdp_mline_index: c.sdp_mline_index,
                    username_fragment: None,
                };
                match self.pc.add_ice_candidate(init).await {
                    Ok(()) => added += 1,
                    Err(e) => crate::status!("[ice] skipped a candidate the console offered: {e}"),
                }
            }
        }
        if added == 0 {
            return Err(anyhow!("the console offered no usable network candidates"));
        }
        Ok(())
    }

    pub fn state(&self) -> watch::Receiver<RTCPeerConnectionState> {
        self.state.clone()
    }

    pub async fn close(&self) {
        let _ = self.stop.send(true);
        let _ = self.pc.close().await;
    }
}

/// True if this RTP payload carries the first bytes of an IDR picture.
pub fn starts_keyframe(payload: &[u8]) -> bool {
    if payload.len() < 2 {
        return false;
    }
    match payload[0] & 0x1f {
        5 => true,
        // FU-A: only the fragment with the start bit counts.
        28 => payload[1] & 0x80 != 0 && payload[1] & 0x1f == 5,
        // STAP-A: walk the aggregated NAL units (u16 size prefix each).
        24 => {
            let mut rest = &payload[1..];
            while rest.len() > 2 {
                let size = usize::from(u16::from_be_bytes([rest[0], rest[1]]));
                if size == 0 || rest.len() < 2 + size {
                    break;
                }
                if rest[2] & 0x1f == 5 {
                    return true;
                }
                rest = &rest[2 + size..];
            }
            false
        }
        _ => false,
    }
}

fn set_video_bitrate(sdp: &str, kbps: u32) -> String {
    if kbps == 0 {
        return sdp.to_owned();
    }
    let mut out = Vec::new();
    let mut in_video = false;
    let mut placed = false;
    for line in sdp.split("\r\n") {
        if line.starts_with("m=") {
            in_video = line.starts_with("m=video");
            placed = false;
        }
        if in_video && line.starts_with("b=") {
            continue;
        }
        out.push(line.to_owned());
        if in_video && !placed && line.starts_with("c=") {
            out.push(format!("b=AS:{kbps}"));
            placed = true;
        }
    }
    out.join("\r\n")
}

/// Extracts the obfuscated IPv4 address and port from a Teredo IPv6 address.
fn teredo_endpoint(address: &str) -> Option<(String, u16)> {
    let groups: Vec<&str> = address.split(':').collect();
    // Teredo is 2001:0000::/32; other 2001: addresses are ordinary IPv6.
    let is_teredo = groups.len() == 8
        && groups[0] == "2001"
        && u16::from_str_radix(groups[1], 16) == Ok(0);
    if !is_teredo {
        return None;
    }
    let port = !u16::from_str_radix(groups[5], 16).ok()?;
    let hi = !u16::from_str_radix(groups[6], 16).ok()?;
    let lo = !u16::from_str_radix(groups[7], 16).ok()?;
    Some((format!("{}.{}.{}.{}", hi >> 8, hi & 0xff, lo >> 8, lo & 0xff), port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitrate_line_goes_after_video_connection_line() {
        let sdp = "v=0\r\nm=audio 9 X\r\nc=IN IP4 0.0.0.0\r\nm=video 9 X\r\nc=IN IP4 0.0.0.0\r\na=mid:1\r\n";
        let out = set_video_bitrate(sdp, 15000);
        assert_eq!(
            out,
            "v=0\r\nm=audio 9 X\r\nc=IN IP4 0.0.0.0\r\nm=video 9 X\r\nc=IN IP4 0.0.0.0\r\nb=AS:15000\r\na=mid:1\r\n"
        );
    }

    #[test]
    fn teredo_address_decodes_to_ipv4() {
        // 192.168.1.20:3074 -> inverted: 3f57:feeb, port f3fd
        let (ip, port) = teredo_endpoint("2001:0:1234:5678:0:f3fd:3f57:feeb").unwrap();
        assert_eq!(ip, "192.168.1.20");
        assert_eq!(port, 3074);
        assert!(teredo_endpoint("192.168.1.20").is_none());
    }
}
