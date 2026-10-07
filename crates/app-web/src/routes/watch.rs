//! `GET /api/sessions`, `POST /api/watch`, `POST /api/watch/stop`.

use super::{detached, detached_op, ensure_ready, ApiError, AppHandle};
use crate::view::{error_kind, sessions_view, watch_end_notice, AppStateView, SessionView};
use axum::extract::State;
use axum::Json;
use holochain_moq_app::lifecycle::{OpId, OpKind};
use holochain_moq_node::{RetryPolicy, SessionDirectory};
use holochain_moq_protocol::SessionId;
use serde::Deserialize;

/// Bounded to roughly two seconds of live video at the existing default 30 fps /
/// keyframe-every-5 encoder settings (see `crates/app`'s `EncodedVideoQueue` docs for
/// why this must stay bounded rather than growing without limit): enough that a normal
/// `DRAIN_INTERVAL` tick (20ms, see `crate::video_ws`) never starves the browser, small
/// enough that a stalled/absent browser cannot make this process's memory grow.
const ENCODED_QUEUE_CAPACITY: usize = 64;

/// A pure query - `App::discover_sessions` requires no particular `state()` and changes
/// none, so this never touches the app's menu/lifecycle state (see its own docs). The
/// existing Stage 14D live/unexpired/endpoint-fresh discovery rule is applied entirely by
/// `holochain_moq_screenshare::joinable_sessions`, called through `App` unchanged; this
/// handler does not re-check or duplicate any of it.
pub async fn get_sessions<D: SessionDirectory + Send + Sync + 'static>(
    State(handle): State<AppHandle<D>>,
) -> Result<Json<Vec<SessionView>>, ApiError> {
    handle.liveness.touch();
    let app = handle.app.lock().await;
    let sessions = app
        .discover_sessions()
        .await
        .map_err(|e| ApiError::internal(format!("discovering live sessions: {e}")))?;
    Ok(Json(sessions_view(&sessions)))
}

#[derive(Debug, Deserialize)]
pub struct WatchRequest {
    pub session_id: String,
}

pub async fn post_watch<D: SessionDirectory + Send + Sync + 'static>(
    State(handle): State<AppHandle<D>>,
    Json(req): Json<WatchRequest>,
) -> Result<Json<AppStateView>, ApiError> {
    // The operation begins here, BEFORE anything else can wait (parsing, the app lock), so a
    // Watch that never starts is still visible as `watch.requested` and where it stopped.
    let lifecycle = handle.lifecycle.clone();
    let op = lifecycle.begin(OpKind::Watch, "POST /api/watch");
    detached_op(
        lifecycle,
        op.clone(),
        "watch.failed",
        watch_inner(handle, req, op),
    )
    .await
}

async fn watch_inner<D: SessionDirectory + Send + Sync + 'static>(
    handle: AppHandle<D>,
    req: WatchRequest,
    op: OpId,
) -> Result<Json<AppStateView>, ApiError> {
    let session_id: SessionId = req
        .session_id
        .parse()
        .map_err(|_| ApiError::bad_request("invalid session id"))?;
    handle.lifecycle.set_session(&op, session_id);

    let waited = std::time::Instant::now();
    let mut app = handle.app.lock().await;
    handle.lifecycle.event(
        &op,
        "http.lock_acquired",
        Some(format!("waited {}ms", waited.elapsed().as_millis())),
    );

    if !app.choose_watch() {
        return Err(ApiError::conflict(
            "cannot start watching: the app is not at Ready (a share or watch is already active)",
        ));
    }
    if !app.enter_selecting() {
        // Cannot happen on the path just taken (`choose_watch` always leaves the app in
        // `Discovering`, which is exactly what `enter_selecting` requires) - handled
        // defensively rather than assumed, matching `App`'s own precondition checks.
        ensure_ready(&mut app).await;
        return Err(ApiError::internal("could not enter session selection"));
    }

    // Stage 14H.5: the decode-free watch (`App::start_watch_encoded`) - this process
    // never decodes the browser's video itself, so there is no `VideoSink`/`Progress`
    // pair to construct here any more (see the README, "Browser video" section, for why
    // the 14H.1 `placeholder_watch_sink` no longer applies). Existence/joinability/
    // expiry/admission are all checked inside `start_watch_encoded`'s own background
    // task (`watch_encoded` -> `find_joinable_session`), exactly as for `start_watch` -
    // not re-checked here. An unknown or no-longer-joinable `session_id` is therefore
    // *not* a synchronous error from this endpoint: it surfaces on a later
    // `GET /api/state` once the background attempt concludes (`watch.finished == true`
    // and, after `POST /api/watch/stop`, the real error text).
    app.adopt_op(op);
    match app
        .start_watch_encoded(session_id, RetryPolicy::default(), ENCODED_QUEUE_CAPACITY)
        .await
    {
        Ok(_queue) => Ok(Json(AppStateView::from_app(&app))),
        Err(e) => {
            ensure_ready(&mut app).await;
            Err(ApiError::from(e))
        }
    }
}

pub async fn post_watch_stop<D: SessionDirectory + Send + Sync + 'static>(
    State(handle): State<AppHandle<D>>,
) -> Result<Json<AppStateView>, ApiError> {
    detached(watch_stop_inner(handle)).await
}

async fn watch_stop_inner<D: SessionDirectory + Send + Sync + 'static>(
    handle: AppHandle<D>,
) -> Result<Json<AppStateView>, ApiError> {
    let mut app = handle.app.lock().await;
    let op = app.watch_op();
    let started = std::time::Instant::now();
    if let Some(op) = &op {
        handle.lifecycle.event(op, "http.stop_received", None);
    }

    if !app.request_stop_watch() {
        return Err(ApiError::bad_request("no active watch to stop"));
    }

    // Blocks only until the background task reports (immediately, if it had already
    // finished on its own) - never touches the publisher (`request_stop_watch` is
    // strictly the local subscriber's own cancellation, unchanged from 14G.3).
    let finished = app.finish_watch().await;
    if let Some(op) = &op {
        handle.lifecycle.event(
            op,
            "http.stop_completed",
            Some(format!("{}ms", started.elapsed().as_millis())),
        );
    }
    match finished {
        Ok(end) => Ok(Json(
            AppStateView::from_app(&app).with_notice(watch_end_notice(end)),
        )),
        Err(e) => Ok(Json(
            AppStateView::from_app(&app)
                .with_last_error(format!("{e:#}"))
                .with_error_kind(error_kind(&e)),
        )),
    }
}
