#ifndef AVFOUNDATION_SHIM_H
#define AVFOUNDATION_SHIM_H

#include <stdint.h>
#include <CoreMedia/CoreMedia.h>

#ifdef __cplusplus
extern "C" {
#endif

// Forward declarations
typedef struct AVFoundationContext AVFoundationContext;

typedef struct VideoPropertiesC {
  int32_t width;
  int32_t height;
  double  duration;
  double  frame_rate;
  int32_t time_scale;
} VideoPropertiesC;

AVFoundationContext* avfoundation_create_context(const char* video_path);
int  avfoundation_get_video_properties(AVFoundationContext* ctx, VideoPropertiesC* props);
void* avfoundation_copy_track_format_desc(AVFoundationContext* ctx);
void* avfoundation_read_next_sample(AVFoundationContext* ctx);
int   avfoundation_get_reader_status(AVFoundationContext* ctx);
int   avfoundation_seek_to(AVFoundationContext* ctx, double timestamp_sec);
/* NEW: explicit start + first pts (debug) */
int   avfoundation_start_reader(AVFoundationContext* ctx);
double avfoundation_peek_first_sample_pts(AVFoundationContext* ctx);
void  avfoundation_release_context(AVFoundationContext* ctx);
/* existing */
const void* avfoundation_create_destination_attributes(void);
const void* avfoundation_create_destination_attributes_scaled(int width, int height);

// Install a global handler that logs any uncaught NSException (name, reason, callstack)
void avf_install_uncaught_exception_handler(void);

// VT wrapper functions
#include <CoreMedia/CoreMedia.h>
#include <VideoToolbox/VideoToolbox.h>
#include <IOSurface/IOSurface.h>

// Call VTDecompressionSessionCreate safely (wraps in @try/@catch).
// 'cb' is the VT output callback; 'refcon' is passed back to that callback.
OSStatus avf_vt_create_session(CMFormatDescriptionRef fmt,
                               CFDictionaryRef dest_attrs,
                               VTDecompressionOutputCallback cb,
                               void *refcon,
                               VTDecompressionSessionRef *out_sess);

// Create VT session with IOSurface destination attributes for zero-copy
OSStatus avf_vt_create_session_iosurface(CMFormatDescriptionRef fmt,
                                          VTDecompressionOutputCallback cb,
                                          void *refcon,
                                          VTDecompressionSessionRef *out_sess);

// Safe wrapper around VTDecompressionSessionDecodeFrame (async).
OSStatus avf_vt_decode_frame(VTDecompressionSessionRef sess,
                             CMSampleBufferRef sb);

// Safe wrappers for session lifecycle/utilities.
void avf_vt_wait_async(VTDecompressionSessionRef sess);
void avf_vt_invalidate(VTDecompressionSessionRef sess);

// IOSurface helper functions
IOSurfaceRef avf_cvpixelbuffer_get_iosurface(void* pixel_buffer);
const void* avf_create_iosurface_destination_attributes(int width, int height);

// --- IOSurface plane helpers (used by wgpu_integration.rs) ---
void avf_iosurface_lock_readonly(IOSurfaceRef surface);
void avf_iosurface_unlock(IOSurfaceRef surface);
size_t avf_iosurface_width_of_plane(IOSurfaceRef surface, size_t plane);
size_t avf_iosurface_height_of_plane(IOSurfaceRef surface, size_t plane);
size_t avf_iosurface_bytes_per_row_of_plane(IOSurfaceRef surface, size_t plane);
void*  avf_iosurface_base_address_of_plane(IOSurfaceRef surface, size_t plane);

#ifdef __cplusplus
}
#endif

#endif // AVFOUNDATION_SHIM_H
