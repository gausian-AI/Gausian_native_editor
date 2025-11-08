//! macOS VideoToolbox implementation
//!
//! This module provides hardware-accelerated video decoding using Apple's VideoToolbox framework.
//! It supports both CPU plane copies (Phase 1) and zero-copy via IOSurface (Phase 2).

use super::*;
use anyhow::Context;
use core_foundation::base::{CFRetain, TCFType};
use core_foundation::string::CFString;
use core_foundation::url::CFURL;
use core_media::format_description::CMVideoDimensions;
use core_media::sample_buffer::CMSampleBuffer;
use core_media::time::{CMTime, CMTimeValue};
use core_media::time_range::CMTimeRange;
use core_video::pixel_buffer::CVPixelBuffer;
use io_surface::IOSurface;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::path::Path;
use std::ptr;
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};

// VideoToolbox bindings
extern "C" {
    fn VTDecompressionSessionCreate(
        allocator: *mut c_void,
        video_format_description: *mut c_void,
        video_decoder_specification: *mut c_void,
        destination_image_buffer_attributes: *mut c_void,
        output_callback: *const c_void, // Correct: const VTDecompressionOutputCallbackRecord*
        decompression_session_out: *mut *mut c_void,
    ) -> i32;

    fn VTDecompressionSessionDecodeFrame(
        session: *mut c_void,
        sample_buffer: *mut c_void,
        flags: u32,
        frame_refcon: *mut c_void,
        info_flags_out: *mut u32,
    ) -> i32;

    fn VTDecompressionSessionInvalidate(session: *mut c_void);

    fn VTDecompressionSessionWaitForAsynchronousFrames(session: *mut c_void) -> i32;

    fn CMFormatDescriptionCreate(
        allocator: *mut c_void,
        media_type: u32,
        media_subtype: u32,
        extensions: *mut c_void,
        format_description_out: *mut *mut c_void,
    ) -> i32;

    fn CMVideoFormatDescriptionGetDimensions(format_description: *mut c_void) -> CMVideoDimensions;

    fn CMTimeGetSeconds(time: CMTime) -> f64;

    fn CMTimeMake(value: CMTimeValue, timescale: i32) -> CMTime;

    fn CMTimeRangeMake(start: CMTime, duration: CMTime) -> CMTimeRange;

    // CoreFoundation functions
    fn CFRelease(cf: *mut c_void);

    // CoreVideo functions
    fn CVPixelBufferLockBaseAddress(pixel_buffer: *mut c_void, lock_flags: u32) -> i32;
    fn CVPixelBufferUnlockBaseAddress(pixel_buffer: *mut c_void, unlock_flags: u32) -> i32;
    fn CVPixelBufferGetWidth(pixel_buffer: *mut c_void) -> u32;
    fn CVPixelBufferGetHeight(pixel_buffer: *mut c_void) -> u32;
    fn CVPixelBufferGetPixelFormatType(pixel_buffer: *mut c_void) -> u32;
    fn CVPixelBufferGetBaseAddressOfPlane(
        pixel_buffer: *mut c_void,
        plane_index: usize,
    ) -> *mut c_void;
    fn CVPixelBufferGetBytesPerRowOfPlane(pixel_buffer: *mut c_void, plane_index: usize) -> usize;
    fn CVPixelBufferGetWidthOfPlane(pixel_buffer: *mut c_void, plane_index: usize) -> usize;
    fn CVPixelBufferGetHeightOfPlane(pixel_buffer: *mut c_void, plane_index: usize) -> usize;
}

// AVFoundation shim interface
#[repr(C)]
struct AVFoundationContext {
    asset: *mut c_void,
    reader: *mut c_void,
    track_output: *mut c_void,
    time_scale: i32,
    nominal_fps: f64,
    timecode_base: f64,
}

#[repr(C)]
struct VideoPropertiesC {
    width: i32,
    height: i32,
    duration: f64,
    frame_rate: f64,
    time_scale: i32,
}

extern "C" {
    fn avfoundation_create_context(video_path: *const c_char) -> *mut AVFoundationContext;
    fn avfoundation_get_video_properties(
        ctx: *mut AVFoundationContext,
        props: *mut VideoPropertiesC,
    ) -> c_int;
    fn avfoundation_copy_track_format_desc(ctx: *mut AVFoundationContext) -> *mut c_void;
    fn avfoundation_read_next_sample(ctx: *mut AVFoundationContext) -> *mut c_void;
    fn avfoundation_get_reader_status(ctx: *mut AVFoundationContext) -> c_int;
    fn avfoundation_seek_to(ctx: *mut AVFoundationContext, timestamp_sec: f64) -> c_int;
    fn avfoundation_start_reader(ctx: *mut AVFoundationContext) -> c_int;
    fn avfoundation_peek_first_sample_pts(ctx: *mut AVFoundationContext) -> f64;
    fn avfoundation_release_context(ctx: *mut AVFoundationContext);
    fn avfoundation_create_destination_attributes() -> *const c_void;
    fn avfoundation_create_destination_attributes_scaled(
        width: c_int,
        height: c_int,
    ) -> *const c_void;

    // Uncaught exception handler
    fn avf_install_uncaught_exception_handler();

    // VT wrappers
    fn avf_vt_create_session(
        fmt: *mut std::ffi::c_void,
        dest_attrs: *const std::ffi::c_void,
        cb: unsafe extern "C" fn(
            *mut std::ffi::c_void,
            *mut std::ffi::c_void,
            i32,
            u32,
            *mut std::ffi::c_void,
            CMTime,
            CMTime,
        ),
        refcon: *mut std::ffi::c_void,
        out_sess: *mut *mut std::ffi::c_void,
    ) -> i32;
    fn avf_vt_create_session_iosurface(
        fmt: *mut std::ffi::c_void,
        cb: unsafe extern "C" fn(
            *mut std::ffi::c_void,
            *mut std::ffi::c_void,
            i32,
            u32,
            *mut std::ffi::c_void,
            CMTime,
            CMTime,
        ),
        refcon: *mut std::ffi::c_void,
        out_sess: *mut *mut std::ffi::c_void,
    ) -> i32;
    fn avf_vt_decode_frame(sess: *mut std::ffi::c_void, sample: *mut std::ffi::c_void) -> i32;
    fn avf_vt_wait_async(sess: *mut std::ffi::c_void);
    fn avf_vt_invalidate(sess: *mut std::ffi::c_void);

    // IOSurface helpers
    fn avf_cvpixelbuffer_get_iosurface(pixel_buffer: *mut c_void) -> *mut c_void;
    fn avf_create_iosurface_destination_attributes(width: c_int, height: c_int) -> *const c_void;
    fn avf_iosurface_lock_readonly(s: *mut c_void);
    fn avf_iosurface_unlock(s: *mut c_void);
    fn avf_iosurface_width_of_plane(s: *mut c_void, plane: usize) -> usize;
    fn avf_iosurface_height_of_plane(s: *mut c_void, plane: usize) -> usize;
    fn avf_iosurface_bytes_per_row_of_plane(s: *mut c_void, plane: usize) -> usize;
    fn avf_iosurface_base_address_of_plane(s: *mut c_void, plane: usize) -> *const c_void;
}

// VideoToolbox constants
const KVT_DECOMPRESSION_SESSION_ERR_INVALID_PROPERTY: i32 = -12900;
const KVT_DECOMPRESSION_SESSION_ERR_BAD_VT_SESSION: i32 = -12901;
const KVT_DECOMPRESSION_SESSION_ERR_CANNOT_TELL_WHEN_DONE: i32 = -12902;
const KVT_DECOMPRESSION_SESSION_ERR_INVALID_PIXEL_BUFFER: i32 = -12903;
const KVT_DECOMPRESSION_SESSION_ERR_INVALID_OPERATION: i32 = -12904;
const KVT_DECOMPRESSION_SESSION_ERR_INTERNAL_ERROR: i32 = -12905;
const KVT_DECOMPRESSION_SESSION_ERR_INVALID_SESSION: i32 = -12906;

// Correct fourcc values: '420v' (NV12 video-range) and 'x420' (10-bit bi-planar video-range)
const K_CV_PIXEL_FORMAT_TYPE_420_Y_P_CB_CR_8_BI_PLANAR_VIDEO_RANGE: u32 = 0x34323076; // '420v'
const K_CV_PIXEL_FORMAT_TYPE_420_Y_P_CB_CR_8_BI_PLANAR_FULL_RANGE: u32 = 0x34323066; // '420f'
const K_CV_PIXEL_FORMAT_TYPE_420_Y_P_CB_CR_10_BI_PLANAR_VIDEO_RANGE: u32 = 0x78343230; // 'x420'

// VTDecompressionOutputCallback structure
#[repr(C)]
struct VTDecompressionOutputCallbackRecord {
    decompression_output_callback: Option<
        unsafe extern "C" fn(
            decompression_output_refcon: *mut c_void,
            source_frame_refcon: *mut c_void,
            status: i32,
            info_flags: u32,
            image_buffer: *mut c_void,
            presentation_time_stamp: CMTime,
            presentation_duration: CMTime,
        ),
    >,
    decompression_output_refcon: *mut c_void,
}

// Decoded frame structure for the callback
#[repr(C)]
#[derive(Clone, Copy)]
struct DecodedFrame {
    pixel_buffer: *mut c_void, // CVPixelBufferRef
    presentation_time: CMTime,
}

// Ring buffer for decoded frames
const MAX_DECODED_FRAMES: usize = 8;

struct DecodedFrameBuffer {
    frames: [Option<DecodedFrame>; MAX_DECODED_FRAMES],
    write_index: usize,
    read_index: usize,
    count: usize,
    // NEW:
    fed_samples: usize, // count of VTDecompressionSessionDecodeFrame() calls that returned success
    cb_frames: usize,   // frames enqueued by VT callback
    last_cb_pts: f64,   // last pts seen by callback (seconds)
}

impl DecodedFrameBuffer {
    fn new() -> Self {
        Self {
            frames: [None; MAX_DECODED_FRAMES],
            write_index: 0,
            read_index: 0,
            count: 0,
            fed_samples: 0,
            cb_frames: 0,
            last_cb_pts: f64::NAN,
        }
    }

    fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn is_full(&self) -> bool {
        self.count == MAX_DECODED_FRAMES
    }

    pub fn len(&self) -> usize {
        self.count
    }

    /// Pop the frame whose pts is <= target and closest to it; fall back to oldest if none <= target.
    /// Returns None only if the buffer is empty.
    pub fn pop_nearest_at_or_before(&mut self, target: f64) -> Option<DecodedFrame> {
        if self.count == 0 {
            return None;
        }

        // linear scan is fine (small ring)
        let mut best_idx: Option<usize> = None;
        let mut best_dt = f64::INFINITY;

        for i in 0..MAX_DECODED_FRAMES {
            if let Some(ref frame) = self.frames[i] {
                let pts = unsafe { CMTimeGetSeconds(frame.presentation_time) };
                if pts <= target {
                    let dt = target - pts;
                    if dt < best_dt {
                        best_dt = dt;
                        best_idx = Some(i);
                    }
                }
            }
        }

        if let Some(i) = best_idx {
            let frame = self.frames[i].take();
            self.read_index = (i + 1) % MAX_DECODED_FRAMES;
            self.count -= 1;
            return frame;
        }

        // nothing <= target: pop oldest non-null
        for i in 0..MAX_DECODED_FRAMES {
            let idx = (self.read_index + i) % MAX_DECODED_FRAMES;
            if self.frames[idx].is_some() {
                let frame = self.frames[idx].take();
                self.read_index = (idx + 1) % MAX_DECODED_FRAMES;
                self.count -= 1;
                return frame;
            }
        }
        None
    }

    fn pop_frame(&mut self) -> Option<DecodedFrame> {
        if self.count == 0 {
            return None;
        }

        let frame = self.frames[self.read_index].take();
        self.read_index = (self.read_index + 1) % MAX_DECODED_FRAMES;
        self.count -= 1;
        frame
    }

    fn peek_frame(&self) -> Option<&DecodedFrame> {
        if self.count == 0 {
            return None;
        }
        self.frames[self.read_index].as_ref()
    }

    fn clear(&mut self) {
        for slot in self.frames.iter_mut() {
            if let Some(frame) = slot.take() {
                if !frame.pixel_buffer.is_null() {
                    unsafe {
                        CFRelease(frame.pixel_buffer);
                    }
                }
            }
        }
        self.write_index = 0;
        self.read_index = 0;
        self.count = 0;
        self.fed_samples = 0;
        self.cb_frames = 0;
        self.last_cb_pts = f64::NAN;
    }
}

/// VideoToolbox decoder implementation
pub struct VideoToolboxDecoder {
    session: *mut c_void,
    iosurface_session: *mut c_void, // Separate session for IOSurface zero-copy
    format_description: *mut c_void,
    destination_attributes: *const c_void,
    properties: VideoProperties,
    config: DecoderConfig,
    current_timestamp: f64,
    frame_cache: Arc<Mutex<Vec<VideoFrame>>>,
    iosurface_cache: Arc<Mutex<Vec<IOSurfaceFrame>>>,
    zero_copy_enabled: bool,
    // AVFoundation integration via Obj-C shim
    avfoundation_ctx: *mut AVFoundationContext,
    video_path: String,
    // Decoded frame buffer for VideoToolbox callback
    decoded_frame_buffer: Arc<Mutex<DecodedFrameBuffer>>,
    iosurface_frame_buffer: Arc<Mutex<DecodedFrameBuffer>>, // Separate buffer for IOSurface frames
    // Raw pointer to balance Arc::into_raw() call
    decoded_frame_buffer_raw: *const std::sync::Mutex<DecodedFrameBuffer>,
    iosurface_frame_buffer_raw: *const std::sync::Mutex<DecodedFrameBuffer>,
    reader_started: bool,
    interactive: bool,
}

unsafe impl Send for VideoToolboxDecoder {}
unsafe impl Sync for VideoToolboxDecoder {}

// VideoToolbox decompression output callback
unsafe extern "C" fn vt_decompression_output_callback(
    decompression_output_refcon: *mut c_void,
    _source_frame_refcon: *mut c_void,
    status: i32,
    _info_flags: u32,
    image_buffer: *mut c_void,
    presentation_time_stamp: CMTime,
    _presentation_duration: CMTime,
) {
    if status != 0 {
        debug!("VideoToolbox decompression callback error: {}", status);
        return;
    }
    if image_buffer.is_null() {
        debug!("VideoToolbox callback received null image buffer");
        return;
    }
    // Get the decoded frame buffer from the refcon (raw pointer to Mutex<DecodedFrameBuffer>)
    let mtx_ptr = decompression_output_refcon as *const Mutex<DecodedFrameBuffer>;
    if mtx_ptr.is_null() {
        debug!("VideoToolbox callback received null refcon");
        return;
    }
    let mtx = &*mtx_ptr;
    if let Ok(mut buffer) = mtx.lock() {
        // CRITICAL: Retain the CVPixelBufferRef before storing it
        CFRetain(image_buffer);
        let decoded_frame = DecodedFrame {
            pixel_buffer: image_buffer,
            presentation_time: presentation_time_stamp,
        };
        if buffer.count < MAX_DECODED_FRAMES {
            let write_index = buffer.write_index;
            buffer.frames[write_index] = Some(decoded_frame);
            buffer.write_index = (write_index + 1) % MAX_DECODED_FRAMES;
            buffer.count += 1;
            // Bump counters
            buffer.cb_frames += 1;
            buffer.last_cb_pts = CMTimeGetSeconds(presentation_time_stamp);
            debug!("VT cb: enqueued frame pts={}", buffer.last_cb_pts);
        } else {
            CFRelease(image_buffer);
            debug!("VideoToolbox decoded frame buffer full, dropping frame and releasing CVPixelBufferRef");
        }
    }
}

// VideoToolbox IOSurface decompression output callback for zero-copy
unsafe extern "C" fn vt_iosurface_decompression_callback(
    decompression_output_refcon: *mut c_void,
    _source_frame_refcon: *mut c_void,
    status: i32,
    _info_flags: u32,
    image_buffer: *mut c_void,
    presentation_time_stamp: CMTime,
    _presentation_duration: CMTime,
) {
    if status != 0 {
        debug!("VideoToolbox IOSurface callback error: {}", status);
        return;
    }
    if image_buffer.is_null() {
        debug!("VideoToolbox IOSurface callback received null image buffer");
        return;
    }

    // Get the IOSurface frame buffer from the refcon
    let mtx_ptr = decompression_output_refcon as *const Mutex<DecodedFrameBuffer>;
    if mtx_ptr.is_null() {
        debug!("VideoToolbox IOSurface callback received null refcon");
        return;
    }
    let mtx = &*mtx_ptr;
    if let Ok(mut buffer) = mtx.lock() {
        // CRITICAL: Retain the CVPixelBufferRef before storing it
        CFRetain(image_buffer);
        let decoded_frame = DecodedFrame {
            pixel_buffer: image_buffer,
            presentation_time: presentation_time_stamp,
        };
        if buffer.count < MAX_DECODED_FRAMES {
            let write_index = buffer.write_index;
            buffer.frames[write_index] = Some(decoded_frame);
            buffer.write_index = (write_index + 1) % MAX_DECODED_FRAMES;
            buffer.count += 1;
            // Bump counters
            buffer.cb_frames += 1;
            buffer.last_cb_pts = CMTimeGetSeconds(presentation_time_stamp);
            debug!("VT IOSurface cb: enqueued frame pts={}", buffer.last_cb_pts);
        } else {
            CFRelease(image_buffer);
            debug!("VideoToolbox IOSurface frame buffer full, dropping frame and releasing CVPixelBufferRef");
        }
    }
}

impl Drop for VideoToolboxDecoder {
    fn drop(&mut self) {
        // Clean up AVFoundation context
        if !self.avfoundation_ctx.is_null() {
            unsafe {
                avfoundation_release_context(self.avfoundation_ctx);
            }
        }
        // Clean up VideoToolbox resources
        if !self.format_description.is_null() {
            unsafe {
                CFRelease(self.format_description);
            }
        }
        if !self.destination_attributes.is_null() {
            unsafe {
                CFRelease(self.destination_attributes as *mut c_void);
            }
        }
        if !self.session.is_null() {
            unsafe {
                // Ensure all callbacks are finished before invalidating
                VTDecompressionSessionWaitForAsynchronousFrames(self.session);
                VTDecompressionSessionInvalidate(self.session);
            }
        }
        if !self.iosurface_session.is_null() {
            unsafe {
                // Ensure all callbacks are finished before invalidating
                VTDecompressionSessionWaitForAsynchronousFrames(self.iosurface_session);
                VTDecompressionSessionInvalidate(self.iosurface_session);
            }
        }
        // Clean up decoded frame buffer
        // The Arc will be automatically dropped, but we need to clean up any remaining CVPixelBuffers
        if let Ok(mut buffer) = self.decoded_frame_buffer.lock() {
            for i in 0..MAX_DECODED_FRAMES {
                if let Some(frame) = buffer.frames[i].take() {
                    if !frame.pixel_buffer.is_null() {
                        unsafe {
                            CFRelease(frame.pixel_buffer);
                        }
                    }
                }
            }
        }
        // Clean up IOSurface frame buffer
        if let Ok(mut buffer) = self.iosurface_frame_buffer.lock() {
            for i in 0..MAX_DECODED_FRAMES {
                if let Some(frame) = buffer.frames[i].take() {
                    if !frame.pixel_buffer.is_null() {
                        unsafe {
                            CFRelease(frame.pixel_buffer);
                        }
                    }
                }
            }
        }
        // CRITICAL: Balance the Arc::into_raw() calls from constructor
        if !self.decoded_frame_buffer_raw.is_null() {
            unsafe {
                let _ = Arc::from_raw(self.decoded_frame_buffer_raw);
            }
        }
        if !self.iosurface_frame_buffer_raw.is_null() {
            unsafe {
                let _ = Arc::from_raw(self.iosurface_frame_buffer_raw);
            }
        }
    }
}

impl VideoToolboxDecoder {
    /// Get ring buffer length for HUD display
    pub fn ring_len(&self) -> usize {
        self.decoded_frame_buffer.lock().unwrap().len()
    }

    /// Get callback frame count for HUD display  
    pub fn cb_frames(&self) -> usize {
        self.decoded_frame_buffer.lock().unwrap().cb_frames
    }

    /// Get last callback PTS for HUD display
    pub fn last_cb_pts(&self) -> f64 {
        self.decoded_frame_buffer.lock().unwrap().last_cb_pts
    }

    /// Get fed samples count for HUD display
    pub fn fed_samples(&self) -> usize {
        self.decoded_frame_buffer.lock().unwrap().fed_samples
    }

    /// Create a new VideoToolbox decoder
    pub fn new<P: AsRef<Path>>(path: P, config: DecoderConfig) -> Result<Self> {
        // Install uncaught exception handler to catch any Obj-C exceptions
        unsafe {
            avf_install_uncaught_exception_handler();
        }

        let path_str = path.as_ref().to_string_lossy().to_string();
        debug!("Creating VideoToolbox decoder for: {}", path_str);

        // Create AVFoundation context using the Obj-C shim
        let c_path =
            CString::new(path_str.clone()).context("Failed to create C string from path")?;

        let avfoundation_ctx = unsafe { avfoundation_create_context(c_path.as_ptr()) };

        if avfoundation_ctx.is_null() {
            return Err(anyhow::anyhow!(
                "Failed to create AVFoundation context for: {}",
                path_str
            ));
        }

        // Get video properties from AVFoundation
        let mut props_c = VideoPropertiesC {
            width: 0,
            height: 0,
            duration: 0.0,
            frame_rate: 0.0,
            time_scale: 0,
        };

        let result = unsafe { avfoundation_get_video_properties(avfoundation_ctx, &mut props_c) };

        if result != 0 {
            unsafe {
                avfoundation_release_context(avfoundation_ctx);
            }
            return Err(anyhow::anyhow!("Failed to get video properties"));
        }

        let properties = VideoProperties {
            width: props_c.width as u32,
            height: props_c.height as u32,
            duration: props_c.duration,
            frame_rate: props_c.frame_rate,
            format: YuvPixFmt::Nv12, // Default format, will be updated based on actual format
        };

        debug!(
            "Created VideoToolbox decoder with properties: {}x{} @ {}fps, duration: {}s",
            properties.width, properties.height, properties.frame_rate, properties.duration
        );

        // Reader will be started explicitly below (once)

        // Get the format description from the video track
        let format_description = unsafe { avfoundation_copy_track_format_desc(avfoundation_ctx) };

        if format_description.is_null() {
            unsafe {
                avfoundation_release_context(avfoundation_ctx);
            }
            return Err(anyhow::anyhow!(
                "Failed to get format description from video track"
            ));
        }

        debug!("Retrieved CMFormatDescriptionRef for VideoToolbox decoder");

        // Create decoded frame buffer
        let decoded_frame_buffer = Arc::new(Mutex::new(DecodedFrameBuffer::new()));
        let iosurface_frame_buffer = Arc::new(Mutex::new(DecodedFrameBuffer::new()));

        // Create destination attributes
        let destination_attributes = Self::create_destination_attributes()?;

        // Store raw pointers to balance Arc::into_raw() calls
        let decoded_frame_buffer_raw = Arc::into_raw(decoded_frame_buffer.clone())
            as *const std::sync::Mutex<DecodedFrameBuffer>;
        let iosurface_frame_buffer_raw = Arc::into_raw(iosurface_frame_buffer.clone())
            as *const std::sync::Mutex<DecodedFrameBuffer>;

        // Create VTDecompressionSession using wrapper
        let mut session: *mut c_void = ptr::null_mut();
        let status = unsafe {
            avf_vt_create_session(
                format_description as *mut _,
                destination_attributes as *const _,
                vt_decompression_output_callback,
                decoded_frame_buffer_raw as *mut _,
                &mut session as *mut _ as *mut *mut _,
            )
        };

        if status != 0 {
            unsafe {
                avfoundation_release_context(avfoundation_ctx);
            }
            return Err(anyhow::anyhow!(
                "Failed to create VTDecompressionSession: {}",
                status
            ));
        }

        if session.is_null() {
            unsafe {
                avfoundation_release_context(avfoundation_ctx);
            }
            return Err(anyhow::anyhow!(
                "VTDecompressionSession creation returned null"
            ));
        }

        // Create IOSurface session for zero-copy if enabled
        let mut iosurface_session: *mut c_void = ptr::null_mut();
        if config.zero_copy {
            let iosurface_status = unsafe {
                avf_vt_create_session_iosurface(
                    format_description as *mut _,
                    vt_iosurface_decompression_callback,
                    iosurface_frame_buffer_raw as *mut c_void,
                    &mut iosurface_session as *mut *mut c_void,
                )
            };

            if iosurface_status != 0 {
                debug!("Failed to create IOSurface VTDecompressionSession: {}, falling back to CPU mode", iosurface_status);
                iosurface_session = ptr::null_mut();
            } else {
                debug!(
                    "Successfully created IOSurface VTDecompressionSession for zero-copy decoding"
                );
            }
        }

        let mut dec = Self {
            session,
            iosurface_session,
            format_description,
            destination_attributes,
            properties,
            config: config.clone(),
            current_timestamp: 0.0,
            frame_cache: Arc::new(Mutex::new(Vec::new())),
            iosurface_cache: Arc::new(Mutex::new(Vec::new())),
            zero_copy_enabled: config.zero_copy && !iosurface_session.is_null(),
            avfoundation_ctx,
            video_path: path_str,
            decoded_frame_buffer,
            iosurface_frame_buffer,
            decoded_frame_buffer_raw,
            iosurface_frame_buffer_raw,
            reader_started: false,
            interactive: false,
        };
        // Start AVFoundation reader exactly once
        let start_ok = unsafe { avfoundation_start_reader(dec.avfoundation_ctx) };
        debug!("AVF startReading -> {}", start_ok);
        let first_pts = unsafe { avfoundation_peek_first_sample_pts(dec.avfoundation_ctx) };
        debug!("AVF first sample pts = {}", first_pts);
        dec.reader_started = true;
        Ok(dec)
    }

    /// Create destination image buffer attributes
    fn create_destination_attributes() -> Result<*const c_void> {
        debug!("Creating destination attributes via Objective-C shim");
        Self::create_destination_attributes_internal(None)
    }

    fn create_destination_attributes_scaled(width: i32, height: i32) -> Result<*const c_void> {
        debug!(
            "Creating scaled destination attributes via Objective-C shim ({}x{})",
            width, height
        );
        Self::create_destination_attributes_internal(Some((width, height)))
    }

    fn create_destination_attributes_internal(dims: Option<(i32, i32)>) -> Result<*const c_void> {
        let attributes = unsafe {
            match dims {
                Some((w, h)) => avfoundation_create_destination_attributes_scaled(w, h),
                None => avfoundation_create_destination_attributes(),
            }
        };

        if attributes.is_null() {
            return Err(anyhow::anyhow!("Failed to create destination attributes"));
        }

        debug!("Successfully created destination attributes");
        Ok(attributes)
    }

    fn apply_interactive_mode(&mut self, interactive: bool) -> Result<()> {
        if self.interactive == interactive {
            return Ok(());
        }

        // Determine scaled dimensions when entering interactive mode
        let (scaled_w, scaled_h) = if interactive {
            let scale = 0.5_f32;
            let mut w = (self.properties.width as f32 * scale).round() as i32;
            let mut h = (self.properties.height as f32 * scale).round() as i32;
            w = w.max(2);
            h = h.max(2);
            if w % 2 != 0 {
                w -= 1;
            }
            if h % 2 != 0 {
                h -= 1;
            }
            (w, h)
        } else {
            (0, 0)
        };

        let new_attrs = if interactive {
            Self::create_destination_attributes_scaled(scaled_w, scaled_h)?
        } else {
            Self::create_destination_attributes()?
        };

        unsafe {
            if !self.session.is_null() {
                VTDecompressionSessionWaitForAsynchronousFrames(self.session);
                VTDecompressionSessionInvalidate(self.session);
                self.session = ptr::null_mut();
            }
        }

        let old_attrs = self.destination_attributes;
        let mut session: *mut c_void = ptr::null_mut();
        let status = unsafe {
            avf_vt_create_session(
                self.format_description as *mut _,
                new_attrs as *const _,
                vt_decompression_output_callback,
                self.decoded_frame_buffer_raw as *mut _,
                &mut session as *mut *mut _,
            )
        };

        if status != 0 || session.is_null() {
            unsafe {
                CFRelease(new_attrs as *mut c_void);
            }
            return Err(anyhow::anyhow!(
                "Failed to recreate VTDecompressionSession (status={})",
                status
            ));
        }

        unsafe {
            if !old_attrs.is_null() {
                CFRelease(old_attrs as *mut c_void);
            }
        }

        self.session = session;
        self.destination_attributes = new_attrs;
        self.interactive = interactive;

        if interactive {
            self.zero_copy_enabled = false;
        } else {
            self.zero_copy_enabled = self.config.zero_copy && !self.iosurface_session.is_null();
        }

        if let Ok(mut buffer) = self.decoded_frame_buffer.lock() {
            buffer.clear();
        }
        if let Ok(mut buffer) = self.iosurface_frame_buffer.lock() {
            buffer.clear();
        }
        if let Ok(mut cache) = self.frame_cache.lock() {
            cache.clear();
        }
        if let Ok(mut cache) = self.iosurface_cache.lock() {
            cache.clear();
        }

        debug!("VideoToolbox interactive mode set to {}", interactive);
        Ok(())
    }

    /// Convert CVPixelBufferRef to VideoFrame
    fn cvpixelbuffer_to_videoframe(
        pixel_buffer: *mut c_void,
        timestamp: f64,
    ) -> Result<VideoFrame> {
        if pixel_buffer.is_null() {
            return Err(anyhow::anyhow!("CVPixelBufferRef is null"));
        }

        // Lock the pixel buffer for reading
        let lock_result = unsafe { CVPixelBufferLockBaseAddress(pixel_buffer, 0) };

        if lock_result != 0 {
            return Err(anyhow::anyhow!(
                "Failed to lock CVPixelBuffer: {}",
                lock_result
            ));
        }

        // Get dimensions
        let width = unsafe { CVPixelBufferGetWidth(pixel_buffer) } as u32;
        let height = unsafe { CVPixelBufferGetHeight(pixel_buffer) } as u32;

        // Get pixel format
        let pixel_format = unsafe { CVPixelBufferGetPixelFormatType(pixel_buffer) };

        debug!(
            "CVPixelBuffer: {}x{}, format: 0x{:x}",
            width, height, pixel_format
        );

        let result = match pixel_format {
            K_CV_PIXEL_FORMAT_TYPE_420_Y_P_CB_CR_8_BI_PLANAR_VIDEO_RANGE
            | K_CV_PIXEL_FORMAT_TYPE_420_Y_P_CB_CR_8_BI_PLANAR_FULL_RANGE => {
                Self::extract_nv12_planes(pixel_buffer, width, height, timestamp)
            }
            K_CV_PIXEL_FORMAT_TYPE_420_Y_P_CB_CR_10_BI_PLANAR_VIDEO_RANGE => {
                Self::extract_p010_planes(pixel_buffer, width, height, timestamp)
            }
            _ => {
                warn!(
                    "Unsupported pixel format: 0x{:x}, falling back to test pattern",
                    pixel_format
                );
                Self::generate_test_pattern(width, height, timestamp)
            }
        };

        // Unlock the pixel buffer
        unsafe {
            CVPixelBufferUnlockBaseAddress(pixel_buffer, 0);
        }

        result
    }

    /// Extract NV12 planes from CVPixelBuffer
    fn extract_nv12_planes(
        pixel_buffer: *mut c_void,
        width: u32,
        height: u32,
        timestamp: f64,
    ) -> Result<VideoFrame> {
        // Get Y plane (plane 0)
        let y_base = unsafe { CVPixelBufferGetBaseAddressOfPlane(pixel_buffer, 0) };
        let y_bytes_per_row =
            unsafe { CVPixelBufferGetBytesPerRowOfPlane(pixel_buffer, 0) } as usize;
        let y_width = unsafe { CVPixelBufferGetWidthOfPlane(pixel_buffer, 0) } as usize;
        let y_height = unsafe { CVPixelBufferGetHeightOfPlane(pixel_buffer, 0) } as usize;

        // Get UV plane (plane 1)
        let uv_base = unsafe { CVPixelBufferGetBaseAddressOfPlane(pixel_buffer, 1) };
        let uv_bytes_per_row =
            unsafe { CVPixelBufferGetBytesPerRowOfPlane(pixel_buffer, 1) } as usize;
        let uv_width = unsafe { CVPixelBufferGetWidthOfPlane(pixel_buffer, 1) } as usize;
        let uv_height = unsafe { CVPixelBufferGetHeightOfPlane(pixel_buffer, 1) } as usize;

        debug!(
            "NV12 Y plane: {}x{}, pitch: {}",
            y_width, y_height, y_bytes_per_row
        );
        debug!(
            "NV12 UV plane: {}x{}, pitch: {}",
            uv_width, uv_height, uv_bytes_per_row
        );

        if y_base.is_null() || uv_base.is_null() {
            return Err(anyhow::anyhow!("Failed to get plane base addresses"));
        }

        // Copy Y plane (1 byte per pixel, tightly packed)
        let y_size = (width * height) as usize;
        let mut y_plane = vec![0u8; y_size];

        unsafe {
            let y_src = std::slice::from_raw_parts(y_base as *const u8, y_height * y_bytes_per_row);
            for y in 0..y_height {
                let src_row_start = y * y_bytes_per_row;
                let src_row_end = src_row_start + y_width;
                let dst_row_start = y * y_width;
                let dst_row_end = dst_row_start + y_width;

                if src_row_end <= y_src.len() && dst_row_end <= y_plane.len() {
                    y_plane[dst_row_start..dst_row_end]
                        .copy_from_slice(&y_src[src_row_start..src_row_end]);
                }
            }
        }

        // Copy UV plane (2 bytes per pixel, tightly packed)
        // NV12 UV plane: each row has width bytes (not width/2), but height is height/2
        let uv_size = (width * height / 2) as usize;
        let mut uv_plane = vec![0u8; uv_size];

        unsafe {
            let uv_src =
                std::slice::from_raw_parts(uv_base as *const u8, uv_height * uv_bytes_per_row);
            for y in 0..uv_height {
                let src_row_start = y * uv_bytes_per_row;
                let src_row_end = src_row_start + uv_width;
                let dst_row_start = y * uv_width;
                let dst_row_end = dst_row_start + uv_width;

                if src_row_end <= uv_src.len() && dst_row_end <= uv_plane.len() {
                    uv_plane[dst_row_start..dst_row_end]
                        .copy_from_slice(&uv_src[src_row_start..src_row_end]);
                }
            }
        }

        Ok(VideoFrame {
            width,
            height,
            y_plane,
            uv_plane,
            format: YuvPixFmt::Nv12,
            timestamp,
        })
    }

    /// Extract P010 planes from CVPixelBuffer (10-bit)
    fn extract_p010_planes(
        pixel_buffer: *mut c_void,
        width: u32,
        height: u32,
        timestamp: f64,
    ) -> Result<VideoFrame> {
        // Get Y plane (plane 0) - 16-bit per pixel
        let y_base = unsafe { CVPixelBufferGetBaseAddressOfPlane(pixel_buffer, 0) };
        let y_bytes_per_row =
            unsafe { CVPixelBufferGetBytesPerRowOfPlane(pixel_buffer, 0) } as usize;
        let y_width = unsafe { CVPixelBufferGetWidthOfPlane(pixel_buffer, 0) } as usize;
        let y_height = unsafe { CVPixelBufferGetHeightOfPlane(pixel_buffer, 0) } as usize;

        // Get UV plane (plane 1) - 32-bit per pixel (2x16-bit)
        let uv_base = unsafe { CVPixelBufferGetBaseAddressOfPlane(pixel_buffer, 1) };
        let uv_bytes_per_row =
            unsafe { CVPixelBufferGetBytesPerRowOfPlane(pixel_buffer, 1) } as usize;
        let uv_width = unsafe { CVPixelBufferGetWidthOfPlane(pixel_buffer, 1) } as usize;
        let uv_height = unsafe { CVPixelBufferGetHeightOfPlane(pixel_buffer, 1) } as usize;

        debug!(
            "P010 Y plane: {}x{}, pitch: {}",
            y_width, y_height, y_bytes_per_row
        );
        debug!(
            "P010 UV plane: {}x{}, pitch: {}",
            uv_width, uv_height, uv_bytes_per_row
        );

        if y_base.is_null() || uv_base.is_null() {
            return Err(anyhow::anyhow!("Failed to get plane base addresses"));
        }

        // Copy Y plane (2 bytes per pixel, tightly packed)
        let y_size = (width * height * 2) as usize; // 16-bit per pixel
        let mut y_plane = vec![0u8; y_size];

        unsafe {
            let y_src = std::slice::from_raw_parts(y_base as *const u8, y_height * y_bytes_per_row);
            for y in 0..y_height {
                let src_row_start = y * y_bytes_per_row;
                let src_row_end = src_row_start + (y_width * 2); // 2 bytes per pixel
                let dst_row_start = y * (y_width * 2);
                let dst_row_end = dst_row_start + (y_width * 2);

                if src_row_end <= y_src.len() && dst_row_end <= y_plane.len() {
                    y_plane[dst_row_start..dst_row_end]
                        .copy_from_slice(&y_src[src_row_start..src_row_end]);
                }
            }
        }

        // Copy UV plane (4 bytes per pixel, tightly packed)
        let uv_size = (width * height) as usize; // 2x16-bit per pixel = 4 bytes per pixel
        let mut uv_plane = vec![0u8; uv_size];

        unsafe {
            let uv_src =
                std::slice::from_raw_parts(uv_base as *const u8, uv_height * uv_bytes_per_row);
            for y in 0..uv_height {
                let src_row_start = y * uv_bytes_per_row;
                let src_row_end = src_row_start + (uv_width * 2); // 2 bytes per pixel
                let dst_row_start = y * (uv_width * 2);
                let dst_row_end = dst_row_start + (uv_width * 2);

                if src_row_end <= uv_src.len() && dst_row_end <= uv_plane.len() {
                    uv_plane[dst_row_start..dst_row_end]
                        .copy_from_slice(&uv_src[src_row_start..src_row_end]);
                }
            }
        }

        Ok(VideoFrame {
            width,
            height,
            y_plane,
            uv_plane,
            format: YuvPixFmt::P010,
            timestamp,
        })
    }

    /// Generate test pattern (fallback)
    fn generate_test_pattern(width: u32, height: u32, timestamp: f64) -> Result<VideoFrame> {
        let y_size = (width * height) as usize;
        let uv_size = (width * height / 2) as usize;

        // Generate animated test pattern
        let time = timestamp * 2.0; // Speed up animation
        let mut y_plane = vec![0u8; y_size];
        let mut uv_plane = vec![128u8; uv_size]; // Neutral chroma

        for y in 0..height {
            for x in 0..width {
                let idx = (y * width + x) as usize;

                // Create a rotating gradient pattern
                let center_x = width as f64 / 2.0;
                let center_y = height as f64 / 2.0;
                let dx = x as f64 - center_x;
                let dy = y as f64 - center_y;
                let angle = dy.atan2(dx) + time;
                let distance = (dx * dx + dy * dy).sqrt();

                // Y component: rotating gradient
                let y_val = ((angle.cos() * 0.5 + 0.5) * 255.0) as u8;
                y_plane[idx] = y_val;

                // UV component: distance-based pattern
                if idx < uv_size {
                    let uv_val = ((distance / 100.0).sin() * 127.0 + 128.0) as u8;
                    uv_plane[idx] = uv_val;
                }
            }
        }

        Ok(VideoFrame {
            width,
            height,
            y_plane,
            uv_plane,
            format: YuvPixFmt::Nv12,
            timestamp,
        })
    }

    /// Seek to a specific timestamp
    pub fn seek_to(&mut self, timestamp: f64) -> Result<()> {
        debug!("Seeking to timestamp: {}", timestamp);

        if self.avfoundation_ctx.is_null() {
            return Err(anyhow::anyhow!("AVFoundation context is null"));
        }

        let result = unsafe { avfoundation_seek_to(self.avfoundation_ctx, timestamp) };

        if result != 0 {
            return Err(anyhow::anyhow!(
                "Failed to seek to timestamp: {}",
                timestamp
            ));
        }

        self.current_timestamp = timestamp;
        // After a seek, we must start reader again exactly once
        let start_ok = unsafe { avfoundation_start_reader(self.avfoundation_ctx) };
        debug!("AVF restart after seek -> {}", start_ok);
        self.reader_started = true;
        debug!("Seek completed to timestamp: {}", timestamp);
        Ok(())
    }

    /// Decode frame with CPU plane copies (Phase 1)
    fn decode_frame_cpu(&mut self, timestamp: f64) -> Result<Option<VideoFrame>> {
        debug!(
            "Decoding frame at timestamp: {} for video: {}",
            timestamp, self.video_path
        );

        // Check if we have a cached frame
        if let Ok(mut cache) = self.frame_cache.lock() {
            if let Some(cached_frame) = cache.iter().find(|f| (f.timestamp - timestamp).abs() < 0.1)
            {
                debug!(
                    "Using cached frame at timestamp: {}",
                    cached_frame.timestamp
                );
                return Ok(Some(cached_frame.clone()));
            }
        }

        // Use tolerant selection - never return "no frame" if ring has any data
        let ring_len = { self.decoded_frame_buffer.lock().unwrap().len() };
        if ring_len == 0 {
            // Ring is genuinely empty → producer didn't feed yet, continue to decode more
        } else {
            // Ring has data - use tolerant selection to always get a frame
            let decoded = {
                let mut rb = self.decoded_frame_buffer.lock().unwrap();
                rb.pop_nearest_at_or_before(timestamp).unwrap_or_else(|| {
                    // ring not empty but no suitable <= target (e.g., only future frames):
                    // pop the oldest so UI always shows something
                    rb.pop_nearest_at_or_before(f64::INFINITY)
                        .expect("ring non-empty")
                })
            };

            let frame_time = unsafe { CMTimeGetSeconds(decoded.presentation_time) };
            let video_frame = Self::cvpixelbuffer_to_videoframe(decoded.pixel_buffer, frame_time)?;

            // CRITICAL: Release the CVPixelBufferRef after copying the data
            unsafe {
                CFRelease(decoded.pixel_buffer);
            }

            // Cache the frame
            if let Ok(mut cache) = self.frame_cache.lock() {
                cache.push(video_frame.clone());
                if cache.len() > 10 {
                    cache.remove(0);
                }
            }

            debug!("Using tolerant decoded frame at timestamp: {}", frame_time);
            return Ok(Some(video_frame));
        }

        // No suitable decoded frame available, need to decode more
        // Check if AVFoundation reader is still active
        if self.avfoundation_ctx.is_null() {
            return Err(anyhow::anyhow!("AVFoundation context is null"));
        }

        let reader_status = unsafe { avfoundation_get_reader_status(self.avfoundation_ctx) };
        debug!("VT feed: reader_status={}", reader_status); // 1=Reading

        if reader_status != 1 {
            // AVAssetReaderStatusReading = 1
            debug!(
                "AVFoundation reader status: {}, no more samples",
                reader_status
            );
            return Ok(None); // End of stream
        }

        // Read next sample from AVFoundation
        let sample_buffer = unsafe { avfoundation_read_next_sample(self.avfoundation_ctx) };
        if sample_buffer.is_null() {
            debug!(
                "VT feed: copyNextSampleBuffer returned NULL (status={})",
                reader_status
            );
            return Ok(None); // End of stream
        }

        // Ensure reader has been started (but never start here in CPU decode loop)
        if self.session.is_null() {
            return Err(anyhow::anyhow!("VTDecompressionSession is null"));
        }

        // Create a source frame refcon (we can use the sample buffer itself)
        let _source_frame_refcon = sample_buffer;

        // Decode the frame using wrapper
        let decode_result = unsafe { avf_vt_decode_frame(self.session, sample_buffer) };
        debug!("VT feed: DecodeFrame status={}", decode_result); // 0 = success

        if decode_result == 0 {
            if let Ok(mut b) = self.decoded_frame_buffer.lock() {
                b.fed_samples += 1;
                debug!(
                    "VT feed: fed_samples={}, cb_frames={}, last_cb_pts={}",
                    b.fed_samples, b.cb_frames, b.last_cb_pts
                );
            }
        } else {
            debug!(
                "VTDecompressionSessionDecodeFrame failed: {}",
                decode_result
            );
            // Release the sample buffer
            unsafe {
                CFRelease(sample_buffer);
            }
            return Err(anyhow::anyhow!(
                "VideoToolbox decode failed: {}",
                decode_result
            ));
        }

        // Release the sample buffer
        unsafe {
            CFRelease(sample_buffer);
        }

        // Check if we now have a decoded frame
        if let Ok(mut buffer) = self.decoded_frame_buffer.lock() {
            if let Some(decoded_frame) = buffer.pop_frame() {
                let frame_time = unsafe { CMTimeGetSeconds(decoded_frame.presentation_time) };
                let video_frame =
                    Self::cvpixelbuffer_to_videoframe(decoded_frame.pixel_buffer, frame_time)?;

                // CRITICAL: Release the CVPixelBufferRef after copying the data
                unsafe {
                    CFRelease(decoded_frame.pixel_buffer);
                }

                // Cache the frame
                if let Ok(mut cache) = self.frame_cache.lock() {
                    cache.push(video_frame.clone());
                    if cache.len() > 10 {
                        cache.remove(0);
                    }
                }

                debug!("Decoded frame at timestamp: {}", frame_time);
                return Ok(Some(video_frame));
            }
        }

        // If we get here, the decode didn't produce a frame immediately
        // This can happen with some codecs, so we'll return None and let the caller retry
        debug!("No frame available after decode attempt");
        Ok(None)
    }

    /// Decode frame with zero-copy IOSurface (Phase 2) - internal implementation
    fn decode_frame_zero_copy_internal(
        &mut self,
        timestamp: f64,
    ) -> Result<Option<IOSurfaceFrame>> {
        debug!("Decoding frame with zero-copy at timestamp: {}", timestamp);

        if !self.zero_copy_enabled || self.iosurface_session.is_null() {
            return Ok(None);
        }

        // Check cache first
        if let Ok(mut cache) = self.iosurface_cache.lock() {
            if let Some(cached_frame) = cache.iter().find(|f| (f.timestamp - timestamp).abs() < 0.1)
            {
                return Ok(Some(cached_frame.clone()));
            }
        }

        // Use tolerant selection from IOSurface frame buffer
        let ring_len = { self.iosurface_frame_buffer.lock().unwrap().len() };
        if ring_len > 0 {
            let decoded = {
                let mut rb = self.iosurface_frame_buffer.lock().unwrap();
                rb.pop_nearest_at_or_before(timestamp).unwrap_or_else(|| {
                    rb.pop_nearest_at_or_before(f64::INFINITY)
                        .expect("ring non-empty")
                })
            };

            let frame_time = unsafe { CMTimeGetSeconds(decoded.presentation_time) };

            // Extract IOSurface from CVPixelBuffer
            let iosurface_ref = unsafe { avf_cvpixelbuffer_get_iosurface(decoded.pixel_buffer) };
            if iosurface_ref.is_null() {
                unsafe {
                    CFRelease(decoded.pixel_buffer);
                }
                return Err(anyhow::anyhow!(
                    "Failed to get IOSurface from CVPixelBuffer"
                ));
            }

            // Create IOSurface wrapper from raw pointer
            let iosurface = unsafe { IOSurface::wrap_under_get_rule(iosurface_ref as _) };
            // Use video properties for width and height since IOSurface API doesn't expose them directly
            let width = self.properties.width;
            let height = self.properties.height;

            let iosurface_frame = IOSurfaceFrame {
                surface: iosurface,
                format: YuvPixFmt::Nv12, // IOSurface is typically NV12
                width,
                height,
                timestamp: frame_time,
            };

            // Release the CVPixelBuffer (IOSurface is now managed separately)
            unsafe {
                CFRelease(decoded.pixel_buffer);
            }

            // Cache the frame
            if let Ok(mut cache) = self.iosurface_cache.lock() {
                cache.push(iosurface_frame.clone());
                if cache.len() > 10 {
                    cache.remove(0);
                }
            }

            debug!(
                "Using zero-copy IOSurface frame at timestamp: {}",
                frame_time
            );
            return Ok(Some(iosurface_frame));
        }

        // No suitable decoded frame available, need to decode more
        if self.avfoundation_ctx.is_null() {
            return Err(anyhow::anyhow!("AVFoundation context is null"));
        }

        let reader_status = unsafe { avfoundation_get_reader_status(self.avfoundation_ctx) };
        if reader_status != 1 {
            // AVAssetReaderStatusReading = 1
            debug!(
                "AVFoundation reader status: {}, no more samples for IOSurface",
                reader_status
            );
            return Ok(None); // End of stream
        }

        // Read next sample from AVFoundation
        let sample_buffer = unsafe { avfoundation_read_next_sample(self.avfoundation_ctx) };
        if sample_buffer.is_null() {
            debug!("IOSurface: copyNextSampleBuffer returned NULL");
            return Ok(None); // End of stream
        }

        // Decode the frame using IOSurface session
        let decode_result = unsafe { avf_vt_decode_frame(self.iosurface_session, sample_buffer) };
        debug!("IOSurface VT feed: DecodeFrame status={}", decode_result);

        if decode_result == 0 {
            if let Ok(mut b) = self.iosurface_frame_buffer.lock() {
                b.fed_samples += 1;
                debug!(
                    "IOSurface VT feed: fed_samples={}, cb_frames={}",
                    b.fed_samples, b.cb_frames
                );
            }
        } else {
            debug!(
                "IOSurface VTDecompressionSessionDecodeFrame failed: {}",
                decode_result
            );
            unsafe {
                CFRelease(sample_buffer);
            }
            return Err(anyhow::anyhow!(
                "IOSurface VideoToolbox decode failed: {}",
                decode_result
            ));
        }

        // Release the sample buffer
        unsafe {
            CFRelease(sample_buffer);
        }

        // Check if we now have a decoded IOSurface frame
        if let Ok(mut buffer) = self.iosurface_frame_buffer.lock() {
            if let Some(decoded_frame) = buffer.pop_frame() {
                let frame_time = unsafe { CMTimeGetSeconds(decoded_frame.presentation_time) };

                // Extract IOSurface from CVPixelBuffer
                let iosurface_ref =
                    unsafe { avf_cvpixelbuffer_get_iosurface(decoded_frame.pixel_buffer) };
                if iosurface_ref.is_null() {
                    unsafe {
                        CFRelease(decoded_frame.pixel_buffer);
                    }
                    return Err(anyhow::anyhow!(
                        "Failed to get IOSurface from decoded CVPixelBuffer"
                    ));
                }

                // Create IOSurface wrapper from raw pointer
                let iosurface = unsafe { IOSurface::wrap_under_get_rule(iosurface_ref as _) };
                // Use video properties for width and height since IOSurface API doesn't expose them directly
                let width = self.properties.width;
                let height = self.properties.height;

                let iosurface_frame = IOSurfaceFrame {
                    surface: iosurface,
                    format: YuvPixFmt::Nv12,
                    width,
                    height,
                    timestamp: frame_time,
                };

                // Release the CVPixelBuffer
                unsafe {
                    CFRelease(decoded_frame.pixel_buffer);
                }

                // Cache the frame
                if let Ok(mut cache) = self.iosurface_cache.lock() {
                    cache.push(iosurface_frame.clone());
                    if cache.len() > 10 {
                        cache.remove(0);
                    }
                }

                debug!("Decoded IOSurface frame at timestamp: {}", frame_time);
                return Ok(Some(iosurface_frame));
            }
        }

        // If we get here, the decode didn't produce a frame immediately
        debug!("No IOSurface frame available after decode attempt");
        Ok(None)
    }
}

impl NativeVideoDecoder for VideoToolboxDecoder {
    fn decode_frame(&mut self, timestamp: f64) -> Result<Option<VideoFrame>> {
        self.current_timestamp = timestamp;

        // Always use CPU mode for regular decode_frame calls
        // Zero-copy is only available through decode_frame_zero_copy
        self.decode_frame_cpu(timestamp)
    }

    fn get_properties(&self) -> VideoProperties {
        self.properties.clone()
    }

    fn seek_to(&mut self, timestamp: f64) -> Result<()> {
        // Perform real reader seek via the inherent method (AVFoundation shim)
        VideoToolboxDecoder::seek_to(self, timestamp)?;
        // Clear caches after seek
        if let Ok(mut cache) = self.frame_cache.lock() {
            cache.clear();
        }
        if let Ok(mut cache) = self.iosurface_cache.lock() {
            cache.clear();
        }
        Ok(())
    }

    fn supports_zero_copy(&self) -> bool {
        self.zero_copy_enabled
    }

    fn decode_frame_zero_copy(&mut self, timestamp: f64) -> Result<Option<IOSurfaceFrame>> {
        self.decode_frame_zero_copy_internal(timestamp)
    }

    fn ring_len(&self) -> usize {
        self.ring_len()
    }

    fn cb_frames(&self) -> usize {
        self.cb_frames()
    }

    fn last_cb_pts(&self) -> f64 {
        self.last_cb_pts()
    }

    fn fed_samples(&self) -> usize {
        self.fed_samples()
    }

    fn set_interactive(&mut self, interactive: bool) -> Result<()> {
        self.apply_interactive_mode(interactive)
    }
}

// Drop implementation is already defined above

/// Create a VideoToolbox decoder
pub fn create_videotoolbox_decoder<P: AsRef<Path>>(
    path: P,
    config: DecoderConfig,
) -> Result<Box<dyn NativeVideoDecoder>> {
    let decoder =
        VideoToolboxDecoder::new(path, config).context("Failed to create VideoToolbox decoder")?;

    info!(
        width = decoder.properties.width,
        height = decoder.properties.height,
        zero_copy = decoder.config.zero_copy,
        "native decoder: VideoToolbox initialized"
    );

    Ok(Box::new(decoder))
}

/// Check if VideoToolbox is available
pub fn is_videotoolbox_available() -> bool {
    // For now, always return true on macOS
    // In a real implementation, we would check for VideoToolbox availability
    true
}

/// Convert pixel format to YuvPixFmt (placeholder implementation)
fn pixel_format_to_yuv_format(_format: u32) -> YuvPixFmt {
    // For now, always return NV12 as default
    // In a real implementation, we would map CVPixelFormatType constants
    YuvPixFmt::Nv12
}

/// Convert YuvPixFmt to pixel format (placeholder implementation)
fn yuv_format_to_pixel_format(_format: YuvPixFmt) -> u32 {
    // For now, return a placeholder value
    // In a real implementation, we would return CVPixelFormatType constants
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_videotoolbox_availability() {
        assert!(is_videotoolbox_available());
    }

    #[test]
    fn test_pixel_format_conversion() {
        assert_eq!(pixel_format_to_yuv_format(0), YuvPixFmt::Nv12);
    }

    #[test]
    fn test_yuv_format_conversion() {
        assert_eq!(yuv_format_to_pixel_format(YuvPixFmt::Nv12), 0);
    }
}
