use thiserror::Error;

/// One raw frame: 8-bit planar YUV 4:2:0 ("I420"), tightly packed
/// (`y`: width x height, `u`/`v`: width/2 x height/2). Backends convert from
/// whatever the device produces (e.g. YUYV) to this at the capture boundary, so
/// an encoder never sees a device pixel format or a backend type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawVideoFrame {
    pub width: usize,
    pub height: usize,
    /// Frame counter as assigned by the source (a gap means the source itself
    /// skipped frames).
    pub sequence: u64,
    /// Capture time in milliseconds since the source started. Deterministic for
    /// the synthetic source.
    pub capture_timestamp_ms: u64,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

impl RawVideoFrame {
    /// Check the plane sizes match the dimensions (width and height must be even).
    pub fn validate(&self) -> Result<(), VideoSourceError> {
        if self.width == 0 || self.height == 0 || self.width % 2 != 0 || self.height % 2 != 0 {
            return Err(VideoSourceError::MalformedFrame(format!(
                "dimensions {}x{} must be non-zero and even",
                self.width, self.height
            )));
        }
        let (luma, chroma) = (
            self.width * self.height,
            (self.width / 2) * (self.height / 2),
        );
        if self.y.len() != luma || self.u.len() != chroma || self.v.len() != chroma {
            return Err(VideoSourceError::MalformedFrame(format!(
                "plane sizes {}/{}/{} do not match {}x{} I420",
                self.y.len(),
                self.u.len(),
                self.v.len(),
                self.width,
                self.height
            )));
        }
        Ok(())
    }
}

/// Capture failures, distinct from codec and transport errors. Backend-specific
/// error types stop at the backend crate; they are mapped onto these.
#[derive(Debug, Error)]
pub enum VideoSourceError {
    #[error("no capture device available: {0}")]
    DeviceUnavailable(String),
    #[error("permission denied opening the capture device: {0}")]
    PermissionDenied(String),
    #[error("unsupported capture format: {0}")]
    UnsupportedFormat(String),
    #[error("the capture device disconnected: {0}")]
    DeviceDisconnected(String),
    #[error("capture stopped")]
    Stopped,
    #[error("malformed frame: {0}")]
    MalformedFrame(String),
}
