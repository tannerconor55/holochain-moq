//! `POST /api/share`, `POST /api/share/stop`.
//!
//! Every access-control decision here is the same `AccessConfig` choice
//! `crates/app/src/main.rs`'s own `run_share` already makes from its own prompt - no new
//! admission model, no duplicated 14F logic. `Public`/`Allowed` map 1:1 onto
//! `Sharer::bind`/`Sharer::bind_with_allowed`, unchanged.

use super::{detached, detached_op, ensure_ready, ApiError, AppHandle};
use crate::config::{build_video_source, share_config};
use crate::view::AppStateView;
use axum::extract::State;
use axum::Json;
use holochain_moq_app::lifecycle::{OpId, OpKind};
use holochain_moq_app::AccessConfig;
use holochain_moq_node::SessionDirectory;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ShareRequest {
    /// `"screen"` or `"synthetic"`.
    pub source: String,
    /// `"public"` (default if omitted) or `"allowed"`.
    #[serde(default)]
    pub access: Option<String>,
    /// Only consulted when `access == "allowed"`. An empty list is a real, already
    /// supported 14F state (only the publisher may join - see
    /// `holochain_moq_app::AccessConfig`'s own docs), not "public".
    #[serde(default)]
    pub allowed_agents: Vec<String>,
    /// Optional human-readable title (Stage 15.3), stored in the session's existing `title`
    /// field. Blank = none.
    #[serde(default)]
    pub title: Option<String>,
}

/// Trim, treat blank as "no title", and reject what the session model itself would reject
/// (`MAX_TITLE_BYTES`) plus control characters, *before* the app's state is touched - so a
/// bad title is a clean 400 naming the problem, not a failure half-way through announcing.
fn validate_title(raw: Option<String>) -> Result<Option<String>, ApiError> {
    let Some(raw) = raw else { return Ok(None) };
    let title = raw.trim();
    if title.is_empty() {
        return Ok(None);
    }
    if title.len() > holochain_moq_protocol::session::MAX_TITLE_BYTES {
        return Err(ApiError::bad_request(format!(
            "the title is too long ({} bytes; the limit is {})",
            title.len(),
            holochain_moq_protocol::session::MAX_TITLE_BYTES
        )));
    }
    if title.chars().any(char::is_control) {
        return Err(ApiError::bad_request(
            "the title must not contain control characters",
        ));
    }
    Ok(Some(title.to_string()))
}

pub async fn post_share<D: SessionDirectory + Send + Sync + 'static>(
    State(handle): State<AppHandle<D>>,
    Json(req): Json<ShareRequest>,
) -> Result<Json<AppStateView>, ApiError> {
    // The operation begins here, BEFORE the app lock is awaited, so a request stuck behind a long
    // operation is visible as `share.requested` with no `http.lock_acquired`.
    let lifecycle = handle.lifecycle.clone();
    let op = lifecycle.begin(OpKind::Share, "POST /api/share");
    detached_op(
        lifecycle,
        op.clone(),
        "share.failed",
        share_inner(handle, req, op),
    )
    .await
}

async fn share_inner<D: SessionDirectory + Send + Sync + 'static>(
    handle: AppHandle<D>,
    req: ShareRequest,
    op: OpId,
) -> Result<Json<AppStateView>, ApiError> {
    // Validate the access choice BEFORE touching the app's state: a malformed request
    // must not be able to leave the app anywhere but `Ready`, and an unrecognized `access`
    // value must never silently degrade to `public` (a typo like "private" would otherwise
    // publish the screen to everyone).
    let access = match req.access.as_deref() {
        None | Some("public") => AccessConfig::Public,
        Some("allowed") => AccessConfig::allowed(&req.allowed_agents)
            .map_err(|e| ApiError::bad_request(format!("{e:#}")))?,
        Some(other) => {
            return Err(ApiError::bad_request(format!(
                "unrecognized access {other:?} (expected \"public\" or \"allowed\")"
            )))
        }
    };

    let title = validate_title(req.title)?;

    let waited = std::time::Instant::now();
    let mut app = handle.app.lock().await;
    handle.lifecycle.event(
        &op,
        "http.lock_acquired",
        Some(format!("waited {}ms", waited.elapsed().as_millis())),
    );

    if !app.choose_share() {
        return Err(ApiError::conflict(
            "cannot start a share: the app is not at Ready (a share or watch is already active)",
        ));
    }

    let (source, video) = match build_video_source(&req.source) {
        Ok(v) => v,
        Err(e) => {
            ensure_ready(&mut app).await;
            return Err(e);
        }
    };

    app.adopt_op(op);
    match app
        .start_share(share_config(title), None, source, video, access)
        .await
    {
        Ok(()) => Ok(Json(AppStateView::from_app(&app))),
        Err(e) => {
            // `start_share` already returns the app to `Ready` itself on every failure
            // path (bind/announce) - this is a safety net, not the primary mechanism.
            ensure_ready(&mut app).await;
            Err(ApiError::from(e))
        }
    }
}

pub async fn post_share_stop<D: SessionDirectory + Send + Sync + 'static>(
    State(handle): State<AppHandle<D>>,
) -> Result<Json<AppStateView>, ApiError> {
    detached(share_stop_inner(handle)).await
}

async fn share_stop_inner<D: SessionDirectory + Send + Sync + 'static>(
    handle: AppHandle<D>,
) -> Result<Json<AppStateView>, ApiError> {
    let mut app = handle.app.lock().await;
    let op = app.share_op();
    let started = std::time::Instant::now();
    if let Some(op) = &op {
        handle.lifecycle.event(op, "http.stop_received", None);
    }

    if !app.request_stop() {
        return Err(ApiError::bad_request("no active share to stop"));
    }

    // Blocks only for the existing, bounded Stage 14E drain - the same wait
    // `crates/app`'s own CLI already accepts before reporting "ended".
    let finished = app.finish_share().await;
    if let Some(op) = &op {
        handle.lifecycle.event(
            op,
            "http.stop_completed",
            Some(format!("{}ms", started.elapsed().as_millis())),
        );
    }
    match finished {
        Ok(_objects_published) => Ok(Json(AppStateView::from_app(&app))),
        Err(e) => {
            // The stop itself completed (the app is back at Ready); the underlying share
            // just did not end cleanly (e.g. the encoder failed). Reported, not hidden,
            // but not an HTTP-level failure of the stop request itself.
            Ok(Json(
                AppStateView::from_app(&app).with_last_error(format!("{e:#}")),
            ))
        }
    }
}
