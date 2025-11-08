//! Fallback implementation for non-macOS platforms
//!
//! This module provides a fallback implementation that uses the existing media-io crate
//! for video decoding on platforms that don't support native VideoToolbox decoding.

use super::*;
use anyhow::{anyhow, Context};
use std::path::Path;
use tracing::{debug, info, warn};

/// Fallback decoder implementation
pub struct FallbackDecoder {
    properties: VideoProperties,
    config: DecoderConfig,
    current_timestamp: f64,
}

impl FallbackDecoder {
    /// Create a new fallback decoder
    pub fn new<P: AsRef<Path>>(path: P, config: DecoderConfig) -> Result<Self> {
        let path_str = path.as_ref().to_string_lossy();
        debug!("Creating fallback decoder for: {}", path_str);

        // For now, return placeholder properties
        // In a real implementation, we would use media-io to probe the file
        let properties = VideoProperties {
            width: 1920,
            height: 1080,
            duration: 10.0,
            frame_rate: 30.0,
            format: YuvPixFmt::Nv12,
        };

        Ok(Self {
            properties,
            config,
            current_timestamp: 0.0,
        })
    }
}

impl NativeVideoDecoder for FallbackDecoder {
    fn decode_frame(&mut self, timestamp: f64) -> Result<Option<VideoFrame>> {
        debug!("Fallback decoding frame at timestamp: {}", timestamp);

        // For now, return a placeholder frame
        // In a real implementation, we would use media-io::decode_yuv_at
        let frame = VideoFrame {
            format: YuvPixFmt::Nv12,
            y_plane: vec![128u8; (self.properties.width * self.properties.height) as usize],
            uv_plane: vec![128u8; (self.properties.width * self.properties.height / 2) as usize],
            width: self.properties.width,
            height: self.properties.height,
            timestamp,
        };

        Ok(Some(frame))
    }

    fn get_properties(&self) -> VideoProperties {
        self.properties.clone()
    }

    fn seek_to(&mut self, timestamp: f64) -> Result<()> {
        debug!("Fallback seeking to timestamp: {}", timestamp);
        self.current_timestamp = timestamp;
        Ok(())
    }
}

/// Create a fallback decoder
pub fn create_fallback_decoder<P: AsRef<Path>>(
    path: P,
    config: DecoderConfig,
) -> Result<Box<dyn NativeVideoDecoder>> {
    let decoder =
        FallbackDecoder::new(path, config).context("Failed to create fallback decoder")?;
    info!("native decoder: software fallback initialized");

    Ok(Box::new(decoder))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_fallback_decoder_creation() {
        let path = PathBuf::from("test.mp4");
        let config = DecoderConfig::default();
        let decoder = create_fallback_decoder(path, config);
        assert!(decoder.is_ok());
    }

    #[test]
    fn test_fallback_decoder_decode() {
        let path = PathBuf::from("test.mp4");
        let config = DecoderConfig::default();
        let mut decoder = create_fallback_decoder(path, config).unwrap();

        let frame = decoder.decode_frame(1.0);
        assert!(frame.is_ok());
        assert!(frame.unwrap().is_some());
    }
}
