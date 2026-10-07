use super::{yuyv_to_i420, CameraConfig, CameraInfo};
use async_trait::async_trait;
use holochain_moq_video::{FrameQueue, RawVideoFrame, VideoFrameSource, VideoSourceError};
use std::{
    io,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
    time::Instant,
};
use v4l::{
    buffer::Type, io::mmap::Stream as MmapStream, io::traits::CaptureStream, video::Capture,
    Device, Format, FourCC,
};

/// Frames buffered between the capture thread and the async consumer. Small on
/// purpose: when the consumer lags, the oldest frame is dropped.
const CAPTURE_QUEUE: usize = 2;
const V4L_BUFFERS: u32 = 4;

fn map_io(context: &str, e: io::Error) -> VideoSourceError {
    match e.kind() {
        io::ErrorKind::PermissionDenied => {
            VideoSourceError::PermissionDenied(format!("{context}: {e}"))
        }
        io::ErrorKind::NotFound => VideoSourceError::DeviceUnavailable(format!("{context}: {e}")),
        _ => VideoSourceError::DeviceDisconnected(format!("{context}: {e}")),
    }
}

/// Enumerate V4L2 capture nodes (empty when there is no camera).
pub fn list_devices() -> Vec<CameraInfo> {
    v4l::context::enum_devices()
        .into_iter()
        .map(|n| CameraInfo {
            index: n.index(),
            name: n.name().unwrap_or_default(),
            path: n.path().display().to_string(),
        })
        .collect()
}

pub struct V4lCameraSource {
    queue: Arc<FrameQueue>,
    error: Arc<Mutex<Option<VideoSourceError>>>,
    stop: Arc<AtomicBool>,
    captured: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
    width: usize,
    height: usize,
}

impl V4lCameraSource {
    /// Open the device and start capturing on a dedicated thread. Frames are
    /// delivered as I420; the size is whatever the driver settled on.
    pub fn open(cfg: CameraConfig) -> Result<Self, VideoSourceError> {
        let devices = list_devices();
        let index = match cfg.device_index {
            Some(i) => i,
            None => devices.first().map(|d| d.index).ok_or_else(|| {
                VideoSourceError::DeviceUnavailable("no /dev/video* capture device found".into())
            })?,
        };
        let dev =
            Device::new(index).map_err(|e| map_io(&format!("opening /dev/video{index}"), e))?;

        let requested = Format::new(cfg.width, cfg.height, FourCC::new(b"YUYV"));
        let fmt = dev
            .set_format(&requested)
            .map_err(|e| map_io("setting the capture format", e))?;
        if fmt.fourcc != FourCC::new(b"YUYV") {
            return Err(VideoSourceError::UnsupportedFormat(format!(
                "the device delivers {} instead of YUYV",
                fmt.fourcc
            )));
        }
        if let Ok(mut params) = dev.params() {
            params.interval = v4l::Fraction::new(1, cfg.fps.max(1));
            let _ = dev.set_params(&params); // best effort: the driver keeps its nearest rate
        }
        let (width, height, stride) =
            (fmt.width as usize, fmt.height as usize, fmt.stride as usize);
        if width % 2 != 0 || height % 2 != 0 {
            return Err(VideoSourceError::UnsupportedFormat(format!(
                "the device chose an odd frame size {width}x{height}"
            )));
        }

        let queue = Arc::new(FrameQueue::new(CAPTURE_QUEUE));
        let error = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let captured = Arc::new(AtomicU64::new(0));
        let (q, err, stp, cap) = (queue.clone(), error.clone(), stop.clone(), captured.clone());

        let thread = std::thread::Builder::new()
            .name("v4l-capture".into())
            .spawn(move || {
                let mut stream =
                    match MmapStream::with_buffers(&dev, Type::VideoCapture, V4L_BUFFERS) {
                        Ok(s) => s,
                        Err(e) => {
                            *err.lock().unwrap() = Some(map_io("starting the capture stream", e));
                            q.close();
                            return;
                        }
                    };
                let started = Instant::now();
                let mut sequence = 0u64;
                while !stp.load(Ordering::SeqCst) {
                    match stream.next() {
                        Ok((buf, _meta)) => {
                            let ts = started.elapsed().as_millis() as u64;
                            match yuyv_to_i420(buf, width, height, stride) {
                                Ok((y, u, v)) => {
                                    q.push_drop_oldest(RawVideoFrame {
                                        width,
                                        height,
                                        sequence,
                                        capture_timestamp_ms: ts,
                                        y,
                                        u,
                                        v,
                                    });
                                    sequence += 1;
                                    cap.fetch_add(1, Ordering::SeqCst);
                                }
                                Err(e) => {
                                    *err.lock().unwrap() = Some(e);
                                    break;
                                }
                            }
                        }
                        Err(e) => {
                            *err.lock().unwrap() = Some(map_io("reading a frame", e));
                            break;
                        }
                    }
                }
                q.close();
            })
            .map_err(|e| VideoSourceError::DeviceUnavailable(format!("capture thread: {e}")))?;

        tracing::info!(index, width, height, "v4l camera opened");
        Ok(Self {
            queue,
            error,
            stop,
            captured,
            thread: Some(thread),
            width,
            height,
        })
    }

    /// The frame size the driver actually delivers.
    pub fn frame_size(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    /// Frames read from the device so far.
    pub fn captured_frames(&self) -> u64 {
        self.captured.load(Ordering::SeqCst)
    }

    /// Frames dropped because the consumer fell behind the device.
    pub fn dropped_frames(&self) -> u64 {
        self.queue.dropped()
    }
}

#[async_trait]
impl VideoFrameSource for V4lCameraSource {
    async fn next_frame(&mut self) -> Result<Option<RawVideoFrame>, VideoSourceError> {
        match self.queue.pop().await {
            Some(frame) => Ok(Some(frame)),
            None => match self.error.lock().unwrap().take() {
                Some(e) => Err(e),
                None => Err(VideoSourceError::Stopped),
            },
        }
    }
}

impl Drop for V4lCameraSource {
    /// Stop capturing and release the device: the capture thread exits after
    /// its current frame read (at most one frame period) and is joined here, so
    /// no thread or device handle outlives the source.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.queue.close();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
