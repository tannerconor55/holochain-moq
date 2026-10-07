//! Stage 15.4: diagnostics views over the lifecycle recorder (`holochain_moq_app::lifecycle`).
//!
//! Non-sensitive by construction: operation ids, event names with times, session ids (an
//! application identity, already shown in the UI), phase and phase age, classification kinds.
//! No keys, signatures, certificates, media, request bodies or raw error text ever enter the
//! recorder, so nothing here can leak them. `/api/state` carries only a compact [`DiagView`];
//! the full recent history is `GET /api/diagnostics`, a loopback-only development endpoint.

use crate::routes::AppHandle;
use holochain_moq_app::lifecycle::{OpKind, OpSnapshot};
use holochain_moq_node::SessionDirectory;
use serde::Serialize;

#[derive(Debug, Serialize, PartialEq)]
pub struct EventView {
    pub name: &'static str,
    pub at_ms: u64,
    pub detail: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct OpView {
    pub id: String,
    pub kind: &'static str,
    pub session_id: Option<String>,
    pub elapsed_ms: u64,
    pub phase: &'static str,
    pub phase_age_ms: u64,
    pub terminal: Option<&'static str>,
    pub failure_kind: Option<&'static str>,
    pub events: Vec<EventView>,
}

impl From<&OpSnapshot> for OpView {
    fn from(s: &OpSnapshot) -> Self {
        Self {
            id: s.id.to_string(),
            kind: s.kind.as_str(),
            session_id: s.session_id.clone(),
            elapsed_ms: s.elapsed.as_millis() as u64,
            phase: s.phase,
            phase_age_ms: s.phase_age.as_millis() as u64,
            terminal: s.terminal,
            failure_kind: s.failure_kind,
            events: s
                .events
                .iter()
                .rev()
                .take(40)
                .rev()
                .map(|e| EventView {
                    name: e.name,
                    at_ms: e.at.as_millis() as u64,
                    detail: e.detail.clone(),
                })
                .collect(),
        }
    }
}

/// The compact section of `/api/state`: enough to see *where* an active operation is.
#[derive(Debug, Serialize, PartialEq)]
pub struct DiagView {
    pub op: String,
    pub phase: &'static str,
    pub phase_age_ms: u64,
    pub video_socket: &'static str,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct LivenessView {
    pub last_page_contact_age_ms: u64,
    pub video_sockets_open: usize,
}

/// Stage 15.5: what this process currently holds, so tests (and a person debugging) can see
/// that everything returns to baseline once an operation ends. Every field is read from an
/// existing owner - nothing here is a second source of truth or a metrics system.
#[derive(Debug, Serialize, PartialEq)]
pub struct ResourceView {
    pub active_shares: usize,
    pub active_watches: usize,
    pub video_sockets_open: usize,
    pub encoded_queue_depth: usize,
    pub encoded_queue_capacity: usize,
    pub encoded_queue_dropped: u64,
    /// Lifecycle operations that have not reached a terminal event.
    pub operations_in_flight: usize,
    /// Tokio tasks alive in this runtime (all of them, the server's own included).
    pub runtime_tasks: usize,
}

/// Stage 17A: media-quality measurement, additive to the diagnostics above. `publisher` is the
/// running encoder (only while this process shares), `viewer` the active watch's server-side view
/// (only while it watches); `process` is this process's CPU and memory. Payload bytes only - no
/// JSON, WebSocket, QUIC or TLS framing is counted as media.
#[derive(Debug, Serialize, PartialEq)]
pub struct MediaView {
    pub process: Option<ProcessView>,
    pub publisher: Option<PublisherMediaView>,
    pub viewer: Option<ViewerMediaView>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct ProcessView {
    /// Cumulative user+system CPU seconds of every thread.
    pub cpu_seconds: f64,
    /// CPU since the previous `/api/diagnostics` request, as a percentage of ONE core (400 = four
    /// cores busy); 0 on the first request.
    pub cpu_percent_since_last: f64,
    pub cpu_cores: usize,
    pub rss_mb: f64,
    pub threads: u32,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct PublisherMediaView {
    pub frames_captured: u64,
    pub frames_encoded: u64,
    pub objects_published: u64,
    pub capture_fps: f64,
    pub encode_fps: f64,
    pub output_fps: f64,
    pub encode_avg_ms: f64,
    pub encode_p50_ms: f64,
    pub encode_p95_ms: f64,
    pub encode_max_ms: f64,
    pub queue_wait_avg_ms: f64,
    pub raw_queue_depth: usize,
    pub raw_queue_capacity: usize,
    pub source_dropped: u64,
    pub encoder_queue_dropped: u64,
    pub skipped_frames: u64,
    pub encoded_bytes: u64,
    pub bitrate_bps: f64,
    pub rolling_bitrate_bps: f64,
    pub bytes_per_frame: f64,
    pub keyframes: u64,
    pub keyframe_interval_ms: f64,
    pub avg_keyframe_bytes: f64,
    pub avg_interframe_bytes: f64,
    pub first_object_ms: Option<f64>,
    pub first_keyframe_ms: Option<f64>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct ViewerMediaView {
    pub objects_received: u64,
    pub bytes_received: u64,
    pub keyframes_received: u64,
    pub objects_forwarded: u64,
    pub bytes_forwarded: u64,
    pub queue_dropped: u64,
    pub queue_depth: usize,
    pub queue_capacity: usize,
    pub bitrate_bps: f64,
    pub media_time_ms: u64,
}

/// The previous process reading, for `cpu_percent_since_last`.
static LAST_PROCESS: std::sync::Mutex<Option<holochain_moq_codec_av1::ProcSample>> =
    std::sync::Mutex::new(None);

fn process_view() -> Option<ProcessView> {
    let now = holochain_moq_codec_av1::process_sample()?;
    let prev = LAST_PROCESS.lock().unwrap().replace(now);
    Some(ProcessView {
        cpu_seconds: now.cpu_us as f64 / 1e6,
        cpu_percent_since_last: prev
            .map(|p| holochain_moq_codec_av1::cpu_percent(&p, &now))
            .unwrap_or(0.0),
        cpu_cores: holochain_moq_codec_av1::cpu_count(),
        rss_mb: now.rss_bytes as f64 / 1_048_576.0,
        threads: now.threads,
    })
}

fn ms(d: std::time::Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn media_view<D: SessionDirectory + Send + Sync + 'static>(
    app: &holochain_moq_app::App<D>,
) -> MediaView {
    let publisher = app.share_media().map(|s| PublisherMediaView {
        frames_captured: s.captured_frames,
        frames_encoded: s.encoded_frames,
        objects_published: s.published_objects,
        capture_fps: s.capture_fps,
        encode_fps: s.encode_fps,
        output_fps: s.output_fps,
        encode_avg_ms: ms(s.avg_encode),
        encode_p50_ms: ms(s.p50_encode),
        encode_p95_ms: ms(s.p95_encode),
        encode_max_ms: ms(s.max_encode),
        queue_wait_avg_ms: ms(s.avg_capture_to_encode),
        raw_queue_depth: s.raw_queue_depth,
        raw_queue_capacity: s.raw_queue_capacity,
        source_dropped: s.source_dropped_frames,
        encoder_queue_dropped: s.dropped_raw_frames,
        skipped_frames: s.skipped_frames,
        encoded_bytes: s.encoded_bytes,
        bitrate_bps: s.bitrate_bps,
        rolling_bitrate_bps: s.rolling_bitrate_bps,
        bytes_per_frame: if s.encoded_frames > 0 {
            s.encoded_bytes as f64 / s.encoded_frames as f64
        } else {
            0.0
        },
        keyframes: s.encoder_keyframes,
        keyframe_interval_ms: s.keyframe_interval_ms,
        avg_keyframe_bytes: s.avg_key_bytes,
        avg_interframe_bytes: s.avg_inter_bytes,
        first_object_ms: s.first_object.map(ms),
        first_keyframe_ms: s.first_keyframe.map(ms),
    });
    let viewer = app.encoded_video_queue().map(|q| {
        let v = q.media_snapshot();
        ViewerMediaView {
            objects_received: v.objects_received,
            bytes_received: v.bytes_received,
            keyframes_received: v.keyframes_received,
            objects_forwarded: v.objects_forwarded,
            bytes_forwarded: v.bytes_forwarded,
            queue_dropped: v.queue_dropped,
            queue_depth: v.queue_depth,
            queue_capacity: v.queue_capacity,
            bitrate_bps: v.bitrate_bps,
            media_time_ms: v.media_time_ms,
        }
    });
    MediaView {
        process: process_view(),
        publisher,
        viewer,
    }
}

/// The per-object stage timestamps both ends keep (bounded rings, `unix_us` microseconds),
/// joinable on each object's own media timestamp - served on its own so `/api/diagnostics` stays small.
#[derive(Debug, Serialize, PartialEq)]
pub struct MediaTimings {
    pub publisher: Vec<PublisherTimingView>,
    pub viewer: Vec<ViewerTimingView>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct PublisherTimingView {
    pub media_ts_ms: u64,
    pub key: bool,
    pub bytes: usize,
    pub captured_unix_us: u64,
    pub encode_start_unix_us: u64,
    pub encode_done_unix_us: u64,
    pub handoff_unix_us: Option<u64>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct ViewerTimingView {
    pub media_ts_ms: u64,
    pub key: bool,
    pub bytes: usize,
    pub arrived_unix_us: u64,
    pub forwarded_unix_us: Option<u64>,
}

pub fn media_timings<D: SessionDirectory + Send + Sync + 'static>(
    app: &holochain_moq_app::App<D>,
) -> MediaTimings {
    MediaTimings {
        publisher: app
            .share_media_timings()
            .into_iter()
            .map(|t| PublisherTimingView {
                media_ts_ms: t.media_ts_ms,
                key: t.key,
                bytes: t.bytes,
                captured_unix_us: t.captured_unix_us,
                encode_start_unix_us: t.encode_start_unix_us,
                encode_done_unix_us: t.encode_done_unix_us,
                handoff_unix_us: t.handoff_unix_us,
            })
            .collect(),
        viewer: app
            .encoded_video_queue()
            .map(|q| {
                q.media_timings()
                    .into_iter()
                    .map(|t| ViewerTimingView {
                        media_ts_ms: t.media_ts_ms,
                        key: t.key,
                        bytes: t.bytes,
                        arrived_unix_us: t.arrived_unix_us,
                        forwarded_unix_us: t.forwarded_unix_us,
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Diagnostics {
    pub media: MediaView,
    pub resources: ResourceView,
    pub app_state: &'static str,
    pub liveness: LivenessView,
    pub current_share: Option<OpView>,
    pub current_watch: Option<OpView>,
    pub recent: Vec<OpView>,
}

pub fn diag_view<D: SessionDirectory + Send + Sync + 'static>(
    handle: &AppHandle<D>,
    app: &holochain_moq_app::App<D>,
) -> Option<DiagView> {
    let lc = handle.lifecycle.clone();
    let snap = app
        .watch_op()
        .or_else(|| app.share_op())
        .and_then(|op| lc.snapshot(&op))?;
    Some(DiagView {
        op: snap.id.to_string(),
        phase: snap.phase,
        phase_age_ms: snap.phase_age.as_millis() as u64,
        video_socket: if handle.liveness.video_sockets_open() > 0 {
            "open"
        } else {
            "none"
        },
    })
}

pub fn diagnostics<D: SessionDirectory + Send + Sync + 'static>(
    handle: &AppHandle<D>,
    app: &holochain_moq_app::App<D>,
) -> Diagnostics {
    let lc = &handle.lifecycle;
    let queue = app.encoded_video_queue();
    Diagnostics {
        media: media_view(app),
        resources: ResourceView {
            active_shares: usize::from(app.share_op().is_some()),
            active_watches: usize::from(app.watch_op().is_some()),
            video_sockets_open: handle.liveness.video_sockets_open(),
            encoded_queue_depth: queue.as_ref().map_or(0, |q| q.len()),
            encoded_queue_capacity: queue.as_ref().map_or(0, |q| q.capacity()),
            encoded_queue_dropped: queue.as_ref().map_or(0, |q| q.dropped()),
            operations_in_flight: lc.in_flight(),
            runtime_tasks: tokio::runtime::Handle::current()
                .metrics()
                .num_alive_tasks(),
        },
        app_state: crate::view::AppStateView::from_app(app).state,
        liveness: LivenessView {
            last_page_contact_age_ms: handle.liveness.since_seen().as_millis() as u64,
            video_sockets_open: handle.liveness.video_sockets_open(),
        },
        current_share: lc.current(OpKind::Share).as_ref().map(OpView::from),
        current_watch: lc.current(OpKind::Watch).as_ref().map(OpView::from),
        recent: lc.recent(16).iter().map(OpView::from).collect(),
    }
}

/// One bounded WARN line for an unexpected failure: the operation (where it stopped, how long
/// it was there), plus what the web layer knows - the app state, whether any page is attending,
/// and whether a video socket is open. Never any payload or credential.
pub fn log_failure_snapshot<D: SessionDirectory + Send + Sync + 'static>(
    handle: &AppHandle<D>,
    app: &holochain_moq_app::App<D>,
    op: &holochain_moq_app::lifecycle::OpId,
    why: &str,
) {
    let Some(snap) = handle.lifecycle.snapshot(op) else {
        return;
    };
    tracing::warn!(
        target: "lifecycle",
        why,
        app_state = crate::view::AppStateView::from_app(app).state,
        connection = ?app.watch_connection_state(),
        last_page_contact_age_ms = handle.liveness.since_seen().as_millis() as u64,
        video_sockets_open = handle.liveness.video_sockets_open(),
        snapshot = %snap.to_log_line(),
        "diagnostic snapshot"
    );
}
