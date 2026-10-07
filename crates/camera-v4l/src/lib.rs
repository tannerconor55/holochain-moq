//! The one real camera backend: Video4Linux2 (Linux only).
//!
//! `V4lCameraSource` implements [`holochain_moq_video::VideoFrameSource`]; it
//! converts the device's YUYV frames to I420 at this boundary and maps V4L/io
//! errors onto [`VideoSourceError`]. No V4L type escapes this crate, and this
//! crate knows nothing about codecs, MoQ, peers or Holochain.
//!
//! Requirements: a Linux kernel with V4L2 and a readable `/dev/video*` node
//! (typically membership of the `video` group). **No system libraries**: the
//! `v4l` crate talks to the kernel through ioctls with pre-generated bindings
//! (`v4l2-sys-mit`), so neither compiling nor running needs libv4l, libclang or
//! GStreamer. Only the YUYV (YUY2) pixel format is supported.
//!
//! On other platforms this crate builds but has no backend: [`open`] returns
//! `VideoSourceError::DeviceUnavailable`.

use holochain_moq_video::VideoSourceError;

/// Convert packed YUYV 4:2:2 to planar I420 (4:2:0, chroma averaged over row
/// pairs). `stride` is bytes per source row. Pure function - unit-testable
/// without hardware.
pub fn yuyv_to_i420(
    data: &[u8],
    width: usize,
    height: usize,
    stride: usize,
) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>), VideoSourceError> {
    if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
        return Err(VideoSourceError::MalformedFrame(format!(
            "YUYV dimensions {width}x{height} must be non-zero and even"
        )));
    }
    if stride < width * 2 || data.len() < stride * (height - 1) + width * 2 {
        return Err(VideoSourceError::MalformedFrame(format!(
            "YUYV buffer of {} bytes is too small for {width}x{height} (stride {stride})",
            data.len()
        )));
    }
    let mut y = vec![0u8; width * height];
    let mut u = vec![0u8; (width / 2) * (height / 2)];
    let mut v = vec![0u8; (width / 2) * (height / 2)];
    for row in 0..height {
        let src = &data[row * stride..row * stride + width * 2];
        for (col2, px) in src.chunks_exact(4).enumerate() {
            y[row * width + col2 * 2] = px[0];
            y[row * width + col2 * 2 + 1] = px[2];
        }
    }
    for crow in 0..height / 2 {
        let (top, bottom) = (crow * 2, crow * 2 + 1);
        for ccol in 0..width / 2 {
            let (a, b) = (
                &data[top * stride + ccol * 4..top * stride + ccol * 4 + 4],
                &data[bottom * stride + ccol * 4..bottom * stride + ccol * 4 + 4],
            );
            u[crow * (width / 2) + ccol] = ((a[1] as u16 + b[1] as u16) / 2) as u8;
            v[crow * (width / 2) + ccol] = ((a[3] as u16 + b[3] as u16) / 2) as u8;
        }
    }
    Ok((y, u, v))
}

/// A capture device found on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraInfo {
    pub index: usize,
    pub name: String,
    pub path: String,
}

/// Requested capture mode. The driver may pick the closest supported one; the
/// frames report the size actually delivered.
#[derive(Debug, Clone)]
pub struct CameraConfig {
    /// `/dev/video<N>`; `None` = the first device found.
    pub device_index: Option<usize>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl Default for CameraConfig {
    fn default() -> Self {
        Self {
            device_index: None,
            width: 320,
            height: 240,
            fps: 15,
        }
    }
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{list_devices, V4lCameraSource};

#[cfg(not(target_os = "linux"))]
mod unsupported {
    use super::*;
    pub fn list_devices() -> Vec<CameraInfo> {
        vec![]
    }
    pub struct V4lCameraSource;
    impl V4lCameraSource {
        pub fn open(_: CameraConfig) -> Result<Self, VideoSourceError> {
            Err(VideoSourceError::DeviceUnavailable(
                "the V4L2 backend is Linux-only".into(),
            ))
        }
    }
}
#[cfg(not(target_os = "linux"))]
pub use unsupported::{list_devices, V4lCameraSource};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yuyv_converts_to_i420_with_averaged_chroma() {
        // 4x2 image. Row 0: Y=10,20,30,40  U=100,110 V=200,210
        //            Row 1: Y=11,21,31,41  U=102,112 V=202,212
        let data = [
            10, 100, 20, 200, 30, 110, 40, 210, //
            11, 102, 21, 202, 31, 112, 41, 212,
        ];
        let (y, u, v) = yuyv_to_i420(&data, 4, 2, 8).unwrap();
        assert_eq!(y, [10, 20, 30, 40, 11, 21, 31, 41]);
        assert_eq!(u, [101, 111]);
        assert_eq!(v, [201, 211]);
    }

    #[test]
    fn stride_padding_is_skipped_and_bad_buffers_rejected() {
        let mut data = vec![0u8; 12 * 2];
        data[0] = 7; // Y of (0,0); rows are 12 bytes apart (4 bytes padding)
        data[12] = 9; // Y of (0,1)
        let (y, ..) = yuyv_to_i420(&data, 4, 2, 12).unwrap();
        assert_eq!((y[0], y[4]), (7, 9));
        assert!(yuyv_to_i420(&data[..10], 4, 2, 12).is_err());
        assert!(yuyv_to_i420(&data, 3, 2, 12).is_err());
    }

    #[test]
    fn opening_without_a_device_is_a_clean_typed_error() {
        // On a machine with no camera this is DeviceUnavailable; with one it opens.
        // Either way it must not panic, and the error must be the typed one.
        match V4lCameraSource::open(CameraConfig::default()) {
            Ok(_) => eprintln!("a camera is present on this machine"),
            Err(e) => assert!(
                matches!(
                    e,
                    VideoSourceError::DeviceUnavailable(_)
                        | VideoSourceError::PermissionDenied(_)
                        | VideoSourceError::UnsupportedFormat(_)
                ),
                "{e:?}"
            ),
        }
    }
}
