//! Native video decoder backends for macOS using VideoToolbox
//!
//! This crate provides hardware-accelerated video decoding using Apple's VideoToolbox framework.
//! It supports both CPU plane copies (Phase 1) and zero-copy via IOSurface (Phase 2).

use anyhow::Result;
use tracing::info;
// Define YUV pixel formats locally to avoid cyclic dependency
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum YuvPixFmt {
    Nv12,
    P010,
}
use std::path::Path;
use std::sync::Arc;
use thiserror::Error;

#[cfg(target_os = "macos")]
mod macos;

// Ensure the Obj-C shim static library is linked when this crate is used.
#[cfg(target_os = "macos")]
#[link(name = "avfoundation_shim", kind = "static")]
extern "C" {}

#[cfg(target_os = "macos")]
pub use macos::VideoToolboxDecoder;

#[cfg(not(target_os = "macos"))]
mod fallback;

// Optional GStreamer backend
#[cfg(feature = "gstreamer")]
mod gstreamer_backend;

#[cfg(feature = "gstreamer")]
pub use gstreamer_backend::{
    build_platform_accelerated_pipeline, select_best_decoder, DecoderSelection,
};

mod wgpu_integration;

/// Video frame data with YUV planes
#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub format: YuvPixFmt,
    pub y_plane: Vec<u8>,
    pub uv_plane: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub timestamp: f64,
}

/// IOSurface-based frame for zero-copy rendering
#[cfg(target_os = "macos")]
#[derive(Clone)]
pub struct IOSurfaceFrame {
    pub surface: io_surface::IOSurface,
    pub format: YuvPixFmt,
    pub width: u32,
    pub height: u32,
    pub timestamp: f64,
}

#[cfg(target_os = "macos")]
unsafe impl Send for IOSurfaceFrame {}
#[cfg(target_os = "macos")]
unsafe impl Sync for IOSurfaceFrame {}

/// Native video decoder trait
pub trait NativeVideoDecoder: Send + Sync {
    /// Decode a frame at the specified timestamp
    fn decode_frame(&mut self, timestamp: f64) -> Result<Option<VideoFrame>>;

    /// Decode a frame with zero-copy IOSurface (Phase 2)
    #[cfg(target_os = "macos")]
    fn decode_frame_zero_copy(&mut self, _timestamp: f64) -> Result<Option<IOSurfaceFrame>> {
        // IOSurface zero-copy not yet implemented
        Err(anyhow::anyhow!("IOSurface zero-copy not yet implemented"))
    }

    /// Get video properties
    fn get_properties(&self) -> VideoProperties;

    /// Seek to a specific timestamp
    fn seek_to(&mut self, timestamp: f64) -> Result<()>;

    /// Check if zero-copy mode is supported
    fn supports_zero_copy(&self) -> bool {
        false
    }

    /// Get ring buffer length for HUD display (optional)
    fn ring_len(&self) -> usize {
        0
    }

    /// Get callback frame count for HUD display (optional)
    fn cb_frames(&self) -> usize {
        0
    }

    /// Get last callback PTS for HUD display (optional)
    fn last_cb_pts(&self) -> f64 {
        f64::NAN
    }

    /// Get fed samples count for HUD display (optional)
    fn fed_samples(&self) -> usize {
        0
    }

    /// Hint the decoder about strict paused mode. In strict mode, backends may
    /// switch to paused + accurate preroll seeks; in streaming mode they may
    /// resume PLAYING and prefetch behavior. Default: no-op.
    fn set_strict_paused(&mut self, _strict: bool) {}

    /// Optional fast (key-unit) seek. Default falls back to accurate seek.
    fn seek_to_keyframe(&mut self, timestamp: f64) -> Result<()> {
        self.seek_to(timestamp)
    }

    /// Toggle interactive (reduced-quality) mode. Default: no-op.
    fn set_interactive(&mut self, _interactive: bool) -> Result<()> {
        Ok(())
    }
}

/// Video properties
#[derive(Debug, Clone)]
pub struct VideoProperties {
    pub width: u32,
    pub height: u32,
    pub duration: f64,
    pub frame_rate: f64,
    pub format: YuvPixFmt,
}

/// Decoder configuration
#[derive(Debug, Clone)]
pub struct DecoderConfig {
    /// Enable hardware acceleration
    pub hardware_acceleration: bool,
    /// Preferred pixel format
    pub preferred_format: Option<YuvPixFmt>,
    /// Enable zero-copy mode (IOSurface)
    pub zero_copy: bool,
}

impl Default for DecoderConfig {
    fn default() -> Self {
        Self {
            hardware_acceleration: true,
            preferred_format: None,
            zero_copy: false,
        }
    }
}

/// Return a descriptive string for the decoder the runtime will use.
pub fn describe_platform_decoder() -> Result<String> {
    #[cfg(feature = "gstreamer")]
    {
        return gstreamer_backend::describe_platform_decoder();
    }

    #[cfg(all(not(feature = "gstreamer"), target_os = "macos"))]
    {
        return Ok("VideoToolbox hardware decoder (Apple VideoToolbox)".to_string());
    }

    #[cfg(all(not(feature = "gstreamer"), not(target_os = "macos")))]
    {
        return Ok("Software fallback decoder (FFmpeg/libavcodec)".to_string());
    }

    #[allow(unreachable_code)]
    Ok("Unknown decoder configuration".to_string())
}

/// Create a native video decoder for the given file
pub fn create_decoder<P: AsRef<Path>>(
    path: P,
    config: DecoderConfig,
) -> Result<Box<dyn NativeVideoDecoder>> {
    let path_buf = path.as_ref().to_path_buf();
    // If the GStreamer feature is enabled, prefer it on all platforms for evaluation.
    #[cfg(feature = "gstreamer")]
    {
        #[cfg(target_os = "macos")]
        {
            // Keep VT path when zero_copy requested to preserve IOSurface integration.
            if config.zero_copy {
                info!(
                    path = %path_buf.display(),
                    zero_copy = config.zero_copy,
                    "native decoder: VideoToolbox (zero-copy) selected"
                );
                return macos::create_videotoolbox_decoder(&path_buf, config);
            }
        }
        info!(
            path = %path_buf.display(),
            hw = config.hardware_acceleration,
            zero_copy = config.zero_copy,
            "native decoder: GStreamer backend selected"
        );
        return gstreamer_backend::create_gst_decoder(&path_buf, config);
    }
    #[cfg(target_os = "macos")]
    {
        info!(
            path = %path_buf.display(),
            zero_copy = config.zero_copy,
            "native decoder: VideoToolbox selected"
        );
        macos::create_videotoolbox_decoder(&path_buf, config)
    }

    #[cfg(not(target_os = "macos"))]
    {
        info!(
            path = %path_buf.display(),
            "native decoder: software fallback selected"
        );
        fallback::create_fallback_decoder(&path_buf, config)
    }
}

/// Check if native decoding is available on this platform
pub fn is_native_decoding_available() -> bool {
    // If using GStreamer backend: available when gst::init() succeeds.
    #[cfg(feature = "gstreamer")]
    {
        return gstreamer_backend::is_available();
    }
    #[cfg(target_os = "macos")]
    {
        macos::is_videotoolbox_available()
    }

    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

// Re-export WGPU integration types
pub use wgpu_integration::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decoder_config_default() {
        let config = DecoderConfig::default();
        assert!(config.hardware_acceleration);
        assert!(!config.zero_copy);
        assert!(config.preferred_format.is_none());
    }

    #[test]
    fn test_native_decoding_availability() {
        // This should not panic
        let _available = is_native_decoding_available();
    }
}
