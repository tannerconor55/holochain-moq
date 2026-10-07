// Stage 15.3: presentation logic for the web UI - how sessions, errors and video-area notes
// are *worded*. Pure functions, no DOM, no state: everything here turns a fact the server
// already decided (the typed `last_error_kind`, the session record, the connection state)
// into words. Nothing here decides or infers authorization, joinability or lifecycle.
// Tested with plain Node: `node --test crates/app-web/src/static/ui.test.js`.
(function (root, factory) {
  if (typeof module !== "undefined" && module.exports) {
    module.exports = factory();
  } else {
    root.HolochainMoqUi = factory();
  }
})(typeof self !== "undefined" ? self : this, function () {
  "use strict";

  /// `uhCAkAbCd...wxyz` -> `uhCAkAbC...wxyz`: recognisable, never dominating the row.
  function shortAgent(agent) {
    if (!agent || agent.length <= 16) return agent || "unknown";
    return `${agent.slice(0, 8)}...${agent.slice(-4)}`;
  }

  /// A session UUID is for debugging/copying, so it is shown short.
  function shortId(id) {
    return id ? String(id).slice(0, 8) : "";
  }

  function formatRemaining(ms) {
    if (ms <= 0) return "expired";
    const total = Math.floor(ms / 1000);
    const h = Math.floor(total / 3600);
    const m = Math.floor((total % 3600) / 60);
    const s = total % 60;
    if (h > 0) return `${h}h ${m}m`;
    if (m > 0) return `${m}m ${s}s`;
    return `${s}s`;
  }

  /// How long a watch may sit in "connecting" before the note says it is taking a while.
  const CONNECTING_HINT_MS = 8000;

  const SOURCE_LABELS = { Screen: "Screen", Video: "Video", Audio: "Audio", Music: "Music", Data: "Data" };

  /// The words for one session list row. `s` is a `SessionView` (from `/api/sessions`).
  function describeSession(s, now) {
    const title = s.title && s.title.trim() ? s.title : "Untitled session";
    const expired = s.expires_at - now <= 0;
    return {
      title,
      publisher: shortAgent(s.publisher),
      source: SOURCE_LABELS[s.source] || String(s.source),
      access: s.private ? "Private" : "Public",
      expires: formatRemaining(s.expires_at - now),
      shortId: shortId(s.session_id),
      // Stage 17H (additive): what the row needs to be honest about a session it can no longer
      // vouch for. `expired` only ever *stops offering* the Watch button locally; the server
      // still decides what is joinable.
      isPrivate: !!s.private,
      expired,
      accessNote: s.private
        ? "Private: listed to everyone, but only the publisher's allowed agents receive video. You may be refused."
        : "Public: anyone can watch.",
      actionLabel: expired ? `${title} has expired` : `Watch ${title}`,
    };
  }

  /// The words for a watch's own state (`watch.state` from `/api/state`), for the state chip.
  function describeWatchState(state, attempt) {
    switch (state) {
      case "selecting":
        return { label: "Choosing a session", tone: "info" };
      case "connecting":
        return { label: "Connecting", tone: "info" };
      case "live":
        return { label: "Live", tone: "ok" };
      case "reconnecting":
        return { label: attempt != null ? `Reconnecting (attempt ${attempt})` : "Reconnecting", tone: "warn" };
      case "ended":
        return { label: "Ended", tone: "info" };
      default:
        return { label: state ? String(state) : "-", tone: "info" };
    }
  }

  /// What a person calls the share's source (`share.source` is the API's own token).
  function describeShareSource(source) {
    switch (source) {
      case "screen":
        return "Screen";
      case "synthetic":
        return "Test pattern";
      default:
        return source ? String(source) : "-";
    }
  }

  /// The words and tone for a share's own state (`share.state` from `/api/state`).
  function describeShareState(state) {
    switch (state) {
      case "live":
        return { label: "Live", tone: "ok", icon: "live" };
      case "ending":
        return { label: "Ending...", tone: "warn", icon: "spinner" };
      case "ended":
        return { label: "Ended", tone: "info", icon: "ended" };
      default:
        return { label: state ? String(state) : "-", tone: "info", icon: "info" };
    }
  }

  /// The message for a stop/finish error. `kind` is the server's typed classification
  /// (`last_error_kind`); each maps to one existing Rust error and gets its own wording.
  /// An error with no kind keeps the server's own text rather than being replaced by a
  /// generic one. Deliberately no cryptographic or transport detail for any of them.
  function describeError(kind, message) {
    switch (kind) {
      case "unauthorized":
        return "Access denied: you are not on this private session's allowed list, so the publisher refused you. No video was received.";
      case "expired":
        return "This session has expired and can no longer be joined.";
      case "ended":
        return "This session has already ended.";
      case "not_found":
        return "That session could not be found. It may have ended - refresh the list.";
      case "publisher_unavailable":
        return "The publisher could not be reached. It may have gone offline.";
      case "connection_failed":
        return "The connection to the publisher could not be established, so the watch was closed.";
      default:
        return message;
    }
  }

  /// A watch that ended without an error. Leaving is the viewer's own choice: no message.
  function describeNotice(notice) {
    return notice === "session_ended" ? "The publisher ended the session." : null;
  }

  /// The one-line note under the video, from facts the server and the decoder already
  /// reported (never a second state machine): `watchState` is the server's watch state,
  /// `videoState` the decoder state, `frames` the decoded count, `stalled` whether the video
  /// label already says stalled.
  function videoNote(watchState, videoState, frames, stalled, connectingMs) {
    if (watchState === "reconnecting") {
      return frames > 0
        ? "Reconnecting to the publisher - the last frame is shown until video resumes."
        : "Reconnecting to the publisher...";
    }
    if (frames === 0) {
      if (watchState === "connecting") {
        // Stage 17H: a publisher that has gone offline can stay listed until its session lease ends, and
        // connecting to it takes a while to fail. Say so once it has taken a moment, without promising
        // how long: the outcome (video, or an error) is still the server's to report.
        return connectingMs > CONNECTING_HINT_MS
          ? "Still connecting... If the publisher is offline this ends with an error. You can Leave at any time."
          : "Connecting...";
      }
      return "Waiting for the first frame...";
    }
    if (stalled) return "No video is arriving right now - the last frame is shown.";
    if (videoState === "waiting_keyframe") return "Recovering - waiting for a keyframe...";
    return "";
  }

  // ---- Stage 15.6: browser capability, playback failure, and the one status headline ----

  /// Why this browser cannot show the video (`reason` from `checkVideoSupport` in video.js). The
  /// words are about the browser, never about the network, so it can never be mistaken for an
  /// outage. Chrome/Chromium is named because it is the browser this stream is verified in.
  function describeSupport(reason) {
    const advice = " Video playback is verified in Chrome or Chromium.";
    switch (reason) {
      case "no_websocket":
        return "This browser does not support WebSocket, which the video connection needs." + advice;
      case "insecure_context":
        return "This browser only enables video decoding on a secure page. Open the app at http://127.0.0.1 or http://localhost (or over HTTPS)." + advice;
      case "no_webcodecs":
        return "This browser does not provide WebCodecs, which is needed to decode the video." + advice;
      case "no_video_decoder":
        return "This browser has only part of WebCodecs (no video decoder), so it cannot decode the video." + advice;
      case "av1_unsupported":
        return "This browser cannot decode AV1 video." + advice;
      case "check_failed":
        return "Could not confirm that this browser can decode AV1 video." + advice;
      default:
        return "This browser cannot play the video." + advice;
    }
  }

  /// A decoder that gave up (`failure` = `{kind, message}` from the decoder state machine). The
  /// raw browser message is not the headline; it goes in the diagnostics detail.
  function describePlaybackFailure(failure) {
    if (!failure) return "";
    if (failure.kind === "decoder_unsupported") {
      return "This browser reported that it cannot decode this video stream." + " Video playback is verified in Chrome or Chromium.";
    }
    if (failure.kind === "decoder_unavailable") {
      return "This browser could not start its video decoder." + " Video playback is verified in Chrome or Chromium.";
    }
    return "This browser could not decode the video: its decoder failed repeatedly." + " Video playback is verified in Chrome or Chromium.";
  }

  /// Wording for a failure of the request itself. A rejected request already carries the
  /// server's own message for the person (a validation problem, a conflict); a server fault or
  /// an unreachable app must not surface a raw error string as the message.
  function describeRequestFailure(status, message) {
    if (typeof status === "number" && status >= 400 && status < 500) return message;
    if (status === undefined || status === null || status === 0) {
      return "Cannot reach the local app. Is it still running?";
    }
    return "The local app hit an unexpected problem. See Diagnostics for details.";
  }

  const HEADLINES = {
    starting: ["Starting", "info"],
    ready: ["Ready", "ok"],
    sharing: ["Sharing", "ok"],
    discovering: ["Discovering sessions", "info"],
    connecting: ["Connecting", "info"],
    live: ["Live", "ok"],
    reconnecting: ["Reconnecting", "warn"],
    ended: ["Ended", "info"],
    expired: ["Expired", "warn"],
    unauthorized: ["Access denied", "error"],
    browser_unsupported: ["Browser unsupported", "error"],
    playback_failed: ["Playback failed", "error"],
    error: ["Error", "error"],
  };

  function headline(key, detail) {
    const [label, tone] = HEADLINES[key];
    return { key, label, tone, detail: detail || "" };
  }

  /// The single status headline, from facts other layers already decided (nothing here infers
  /// authorization, liveness or lifecycle): `appState`/`watch`/`share` from `/api/state`,
  /// `lastError*`/`lastNotice` from the same response, `support` from `checkVideoSupport`
  /// (null while unknown), `video` the decoder status, `unreachable` a failed poll. Precedence:
  /// an unreachable app, then what the server says about the operation, with the browser's own
  /// inability to play shown for a watch the server reports running - and "live" only when the
  /// server itself says live.
  function describeStatus(ctx) {
    if (ctx.unreachable) return headline("error", "Cannot reach the local app.");
    const { appState, watch, share, video, support } = ctx;
    if (appState === "starting") return headline("starting");
    if (appState === "watching" && watch) {
      if (watch.finished) return headline("ended", "The session has ended.");
      if (support && support.ok === false) return headline("browser_unsupported", describeSupport(support.reason));
      if (video && video.state === "failed") return headline("playback_failed", describePlaybackFailure(video.failure));
      switch (watch.state) {
        case "selecting":
          return headline("discovering");
        case "connecting":
          return headline("connecting");
        case "live":
          // The server truthfully says live until it notices a silent publisher (~14s of QUIC
          // idle); the frames tell the rest, so the detail says so without changing the state.
          return headline("live", ctx.stalled ? "No video is arriving right now" : "");
        case "reconnecting":
          return headline(
            "reconnecting",
            watch.reconnect_attempt != null ? `Attempt ${watch.reconnect_attempt}` : ""
          );
        case "ended":
          return headline("ended", "The session has ended.");
        default:
          return headline("connecting");
      }
    }
    if (appState === "sharing") {
      return headline("sharing", share && share.state === "ending" ? "Stopping..." : "");
    }
    // Ready: how the last operation ended, if there was one.
    switch (ctx.lastErrorKind) {
      case "unauthorized":
        return headline("unauthorized", describeError("unauthorized", ctx.lastError));
      case "expired":
        return headline("expired", describeError("expired", ctx.lastError));
      case "ended":
        return headline("ended", describeError("ended", ctx.lastError));
      default:
        if (ctx.lastError) return headline("error", describeError(ctx.lastErrorKind, ctx.lastError));
    }
    if (ctx.lastNotice === "session_ended") return headline("ended", describeNotice("session_ended"));
    return headline("ready");
  }

  // ---- Stage 17H: presentation helpers (words/flags only; they decide nothing) ----

  /// The icon (a symbol name in the page's SVG sprite) for each status headline, so a state is
  /// never carried by colour alone: every headline has an icon, a label and a tone.
  const HEADLINE_ICONS = {
    starting: "spinner",
    ready: "check",
    sharing: "live",
    discovering: "spinner",
    connecting: "spinner",
    live: "live",
    reconnecting: "refresh",
    ended: "ended",
    expired: "clock",
    unauthorized: "lock",
    browser_unsupported: "alert",
    playback_failed: "alert",
    error: "alert",
  };
  function headlineIcon(key) {
    return HEADLINE_ICONS[key] || "info";
  }

  /// A last frame is still on screen but is no longer live (reconnecting, or live with no frames
  /// arriving): the stage says so instead of presenting it as live video.
  function videoNotLive(headlineKey, frames, stalled) {
    if (!(frames > 0)) return false;
    return headlineKey === "reconnecting" || (headlineKey === "live" && !!stalled);
  }

  /// No picture at all may be shown: an ended, failed or unsupported area must not leave a frozen
  /// frame that could be mistaken for the stream.
  function videoHidden(headlineKey) {
    return headlineKey === "ended" || headlineKey === "playback_failed" || headlineKey === "browser_unsupported";
  }

  /// The video fit mode is a per-viewer convenience: anything unrecognised is the safe default.
  function normalizeFit(value) {
    return value === "fill" ? "fill" : "fit";
  }

  /// What the video area says over the picture; "" when healthy video is playing.
  function videoOverlay(headlineKey, frames, stalled) {
    switch (headlineKey) {
      case "browser_unsupported":
      case "playback_failed":
        return "Video cannot be shown in this browser";
      case "ended":
        return "The session has ended";
      case "reconnecting":
        return frames > 0 ? "Reconnecting..." : "Reconnecting to the publisher...";
      case "discovering":
      case "connecting":
        return "Connecting...";
      case "live":
        if (frames === 0) return "Waiting for the first frame...";
        return stalled ? "No video is arriving right now" : "";
      default:
        return "";
    }
  }

  return {
    CONNECTING_HINT_MS,
    describeShareState,
    describeWatchState,
    describeShareSource,
    headlineIcon,
    videoNotLive,
    videoHidden,
    normalizeFit,
    describeSupport,
    describePlaybackFailure,
    describeRequestFailure,
    describeStatus,
    videoOverlay,
    shortAgent,
    shortId,
    formatRemaining,
    describeSession,
    describeError,
    describeNotice,
    videoNote,
  };
});
