#import <Foundation/Foundation.h>
#import <AVFoundation/AVFoundation.h>
#import <CoreMedia/CoreMedia.h>
#import <VideoToolbox/VideoToolbox.h>
#import <CoreVideo/CoreVideo.h>
#import <mach/mach_time.h>
#import <stdlib.h>
#import <string.h>
#import "avfoundation_shim.h"

// Debug logging gate (off by default). Enable with GAUSIAN_DEBUG_IOSURFACE=1
static BOOL avf_debug_iosurface = NO;
__attribute__((constructor))
static void avf_read_env(void) {
  const char* v = getenv("GAUSIAN_DEBUG_IOSURFACE");
  avf_debug_iosurface = (v && *v && strcmp(v, "0") != 0);
}
#define AVF_LOG_IOS(fmt, ...) do { if (avf_debug_iosurface) NSLog(fmt, ##__VA_ARGS__); } while (0)

// Init-time verbose logging gate (off by default). Enable with GAUSIAN_DEBUG_AVF=1
static BOOL avf_debug_init = NO;
__attribute__((constructor))
static void avf_read_env2(void) {
  const char* v = getenv("GAUSIAN_DEBUG_AVF");
  avf_debug_init = (v && *v && strcmp(v, "0") != 0);
}
#define AVF_LOG_INIT(fmt, ...) do { if (avf_debug_init) NSLog(fmt, ##__VA_ARGS__); } while (0)

// C interface for AVFoundation operations
// This shim provides a clean C API that Rust can call

static inline void log_exception(const char* func, NSException* e) {
  NSLog(@"[shim] EXC in %@: %@ — %@", [NSString stringWithUTF8String:func], e.name, e.reason);
}

static void AVFUncaughtHandler(NSException *e) {
  NSLog(@"[shim][UNCAUGHT] %@ — %@", e.name, e.reason);
  NSLog(@"[shim][UNCAUGHT] callstack:\n%@", [e callStackSymbols]);
}

void avf_install_uncaught_exception_handler(void) {
  @autoreleasepool {
    NSSetUncaughtExceptionHandler(&AVFUncaughtHandler);
    AVF_LOG_INIT(@"[shim] Uncaught exception handler installed");
  }
}

struct AVFoundationContext {
    void* asset;
    void* reader;
    void* track_output;
    int32_t time_scale;
    double nominal_fps;
    double timecode_base;
    int reader_started;
};

// Create AVFoundation context from video file path
AVFoundationContext* avfoundation_create_context(const char* video_path) {
    @autoreleasepool {
        @try {
            NSString* path = [NSString stringWithUTF8String:video_path];
            NSURL* url = [NSURL fileURLWithPath:path];
            
            if (!url) {
                NSLog(@"Failed to create URL from path: %s", video_path);
                return NULL;
            }
            
            AVURLAsset* asset = [AVURLAsset assetWithURL:url];
            if (!asset) {
                NSLog(@"Failed to create AVURLAsset from URL: %@", url);
                return NULL;
            }
            
            // Get video tracks
            NSArray* videoTracks = [asset tracksWithMediaType:AVMediaTypeVideo];
            if ([videoTracks count] == 0) {
                NSLog(@"No video tracks found in asset");
                return NULL;
            }
            
            AVAssetTrack* videoTrack = [videoTracks objectAtIndex:0];
            if (!videoTrack) {
                NSLog(@"Failed to get video track");
                return NULL;
            }
            
            // Create asset reader
            NSError* error = nil;
            AVAssetReader* reader = [AVAssetReader assetReaderWithAsset:asset error:&error];
            if (!reader) {
                NSLog(@"Failed to create AVAssetReader: %@", error);
                return NULL;
            }
            
            // Create track output for compressed samples (outputSettings: nil)
            AVAssetReaderTrackOutput* trackOutput = [AVAssetReaderTrackOutput 
                assetReaderTrackOutputWithTrack:videoTrack 
                outputSettings:nil];
            
            if (!trackOutput) {
                NSLog(@"Failed to create AVAssetReaderTrackOutput");
                return NULL;
            }
            
            [reader addOutput:trackOutput];
            
            // Allocate context
            AVFoundationContext* ctx = malloc(sizeof(AVFoundationContext));
            if (!ctx) {
                NSLog(@"Failed to allocate AVFoundationContext");
                return NULL;
            }
            
            // Store references (retain them)
            ctx->asset = (__bridge_retained void*)asset;
            ctx->reader = (__bridge_retained void*)reader;
            ctx->track_output = (__bridge_retained void*)trackOutput;
            ctx->time_scale = videoTrack.naturalTimeScale;
            ctx->nominal_fps = videoTrack.nominalFrameRate;
            ctx->timecode_base = 0.0; // Will be set based on first frame
            ctx->reader_started = 0;
            
            AVF_LOG_INIT(@"Created AVFoundation context: time_scale=%d, fps=%.2f", 
                         ctx->time_scale, ctx->nominal_fps);
            
            return ctx;
        } @catch (NSException* e) {
            log_exception(__func__, e);
            return NULL;
        }
    }
}

// Read next sample buffer
void* avfoundation_read_next_sample(AVFoundationContext* ctx) {
    @autoreleasepool {
        @try {
            if (!ctx || !ctx->reader || !ctx->track_output) {
                return NULL;
            }
            
            AVAssetReader* reader = (__bridge AVAssetReader*)ctx->reader;
            AVAssetReaderTrackOutput* trackOutput = (__bridge AVAssetReaderTrackOutput*)ctx->track_output;
            
            if (reader.status != AVAssetReaderStatusReading) {
                NSLog(@"Reader not in reading status: %ld", (long)reader.status);
                return NULL;
            }
            
            CMSampleBufferRef sampleBuffer = [trackOutput copyNextSampleBuffer];
            if (sampleBuffer) {
                // Set timecode base from first frame
                if (ctx->timecode_base == 0.0) {
                    CMTime presentationTime = CMSampleBufferGetPresentationTimeStamp(sampleBuffer);
                    ctx->timecode_base = CMTimeGetSeconds(presentationTime);
                }
            }
            
            return sampleBuffer;
        } @catch (NSException* e) {
            log_exception(__func__, e);
            return NULL;
        }
    }
}

// Seek to timestamp
int avfoundation_seek_to(AVFoundationContext* ctx, double timestamp_sec) {
    @autoreleasepool {
        @try {
            if (!ctx || !ctx->asset || !ctx->reader) {
                return -1;
            }
            
            AVURLAsset* asset = (__bridge AVURLAsset*)ctx->asset;
            AVAssetReader* reader = (__bridge AVAssetReader*)ctx->reader;
            
            // Cancel current reading
            [reader cancelReading];
            
            // Create new reader for the seek position
            NSError* error = nil;
            AVAssetReader* newReader = [AVAssetReader assetReaderWithAsset:asset error:&error];
            if (!newReader) {
                NSLog(@"Failed to create new AVAssetReader for seek: %@", error);
                return -1;
            }
            
            // Get video track
            NSArray* videoTracks = [asset tracksWithMediaType:AVMediaTypeVideo];
            if ([videoTracks count] == 0) {
                NSLog(@"No video tracks found for seek");
                return -1;
            }
            
            AVAssetTrack* videoTrack = [videoTracks objectAtIndex:0];
            
            // Create time range for seek
            CMTime startTime = CMTimeMakeWithSeconds(timestamp_sec, ctx->time_scale);
            CMTime duration = CMTimeMakeWithSeconds(2.0, ctx->time_scale); // 2 second range
            CMTimeRange timeRange = CMTimeRangeMake(startTime, duration);
            
            // Set time range on the reader
            newReader.timeRange = timeRange;
            
            // Create track output for compressed samples (outputSettings: nil)
            AVAssetReaderTrackOutput* trackOutput = [AVAssetReaderTrackOutput 
                assetReaderTrackOutputWithTrack:videoTrack 
                outputSettings:nil];
            
            [newReader addOutput:trackOutput];
            
            // Update context
            ctx->reader = (__bridge_retained void*)newReader;
            ctx->track_output = (__bridge_retained void*)trackOutput;
            ctx->reader_started = 0;
            
            NSLog(@"Seeked to timestamp: %.3f", timestamp_sec);
            return 0;
        } @catch (NSException* e) {
            log_exception(__func__, e);
            return -1000;
        }
    }
}

// Get reader status
int avfoundation_get_reader_status(AVFoundationContext* ctx) {
    @autoreleasepool {
        @try {
            if (!ctx || !ctx->reader) {
                return -1;
            }
            
            AVAssetReader* reader = (__bridge AVAssetReader*)ctx->reader;
            return (int)reader.status;
        } @catch (NSException* e) {
            log_exception(__func__, e);
            return -1000;
        }
    }
}

// Release AVFoundation context
void avfoundation_release_context(AVFoundationContext* ctx) {
    @autoreleasepool {
        @try {
            if (!ctx) {
                return;
            }
            
            if (ctx->asset) {
                CFRelease(ctx->asset);
                ctx->asset = NULL;
            }
            
            if (ctx->reader) {
                CFRelease(ctx->reader);
                ctx->reader = NULL;
            }
            
            if (ctx->track_output) {
                CFRelease(ctx->track_output);
                ctx->track_output = NULL;
            }
            
            free(ctx);
        } @catch (NSException* e) {
            log_exception(__func__, e);
        }
    }
}

int avfoundation_get_video_properties(AVFoundationContext* ctx, VideoPropertiesC* props) {
    @autoreleasepool {
        @try {
            if (!ctx || !ctx->asset || !props) {
                return -1;
            }
            
            AVURLAsset* asset = (__bridge AVURLAsset*)ctx->asset;
            NSArray* videoTracks = [asset tracksWithMediaType:AVMediaTypeVideo];
            
            if ([videoTracks count] == 0) {
                return -1;
            }
            
            AVAssetTrack* videoTrack = [videoTracks objectAtIndex:0];
            
            props->width = (int32_t)videoTrack.naturalSize.width;
            props->height = (int32_t)videoTrack.naturalSize.height;
            props->duration = CMTimeGetSeconds(asset.duration);
            props->frame_rate = videoTrack.nominalFrameRate;
            props->time_scale = videoTrack.naturalTimeScale;
            
            return 0;
        } @catch (NSException* e) {
            log_exception(__func__, e);
            return -1000;
        }
    }
}

// Get CMFormatDescriptionRef for the video track
void* avfoundation_copy_track_format_desc(AVFoundationContext* ctx) {
    @autoreleasepool {
        @try {
            if (!ctx || !ctx->asset) {
                return NULL;
            }
            
            AVURLAsset* asset = (__bridge AVURLAsset*)ctx->asset;
            NSArray* videoTracks = [asset tracksWithMediaType:AVMediaTypeVideo];
            
            if ([videoTracks count] == 0) {
                NSLog(@"No video tracks found for format description");
                return NULL;
            }
            
            AVAssetTrack* videoTrack = [videoTracks objectAtIndex:0];
            id fmtObj = [videoTrack.formatDescriptions lastObject];
            if (!fmtObj) {
                NSLog(@"No format description found for video track");
                return NULL;
            }
            CMFormatDescriptionRef fmt = (__bridge CMFormatDescriptionRef)fmtObj;
            /* Retain before returning across C boundary */
            CFRetain(fmt);
            
            AVF_LOG_INIT(@"Retrieved CMFormatDescriptionRef for track: %dx%d, media type: %s", 
                         (int)videoTrack.naturalSize.width, 
                         (int)videoTrack.naturalSize.height,
                         CMFormatDescriptionGetMediaType(fmt) == kCMMediaType_Video ? "video" : "unknown");
            
            return (void*)fmt;
        } @catch (NSException* e) {
            log_exception(__func__, e);
            return NULL;
        }
    }
}

// Create destination attributes for VideoToolbox decompression
const void* avfoundation_create_destination_attributes(void) {
    return avfoundation_create_destination_attributes_scaled(0, 0);
}

const void* avfoundation_create_destination_attributes_scaled(int width, int height) {
    @autoreleasepool {
        @try {
            NSMutableDictionary* attrs = [@{
                (NSString*)kCVPixelBufferPixelFormatTypeKey : @(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange)
            } mutableCopy];

            if (width > 0 && height > 0) {
                attrs[(NSString*)kCVPixelBufferWidthKey] = @(width);
                attrs[(NSString*)kCVPixelBufferHeightKey] = @(height);
            }

            return (__bridge_retained CFDictionaryRef)attrs;
        } @catch (NSException* e) {
            log_exception(__func__, e);
            return NULL;
        }
    }
}

// NEW: explicit start + first pts (debug)
int avfoundation_start_reader(AVFoundationContext* ctx) {
    @autoreleasepool {
        @try {
            if (!ctx || !ctx->reader) return -1;
            if (ctx->reader_started) {
                return 0; // already started
            }
            AVAssetReader* reader = (__bridge AVAssetReader*)ctx->reader;
            if (reader.status == AVAssetReaderStatusFailed || reader.status == AVAssetReaderStatusCancelled) {
                // Rebuild reader if it is in a bad state
                AVURLAsset* asset = (__bridge AVURLAsset*)ctx->asset;
                NSError* error = nil;
                AVAssetReader* newReader = [AVAssetReader assetReaderWithAsset:asset error:&error];
                if (!newReader) {
                    NSLog(@"[shim] rebuild reader failed: %@", error);
                    return -3;
                }
                // Recreate track output
                NSArray* videoTracks = [asset tracksWithMediaType:AVMediaTypeVideo];
                if ([videoTracks count] == 0) {
                    NSLog(@"No video tracks found when rebuilding reader");
                    return -4;
                }
                AVAssetTrack* videoTrack = [videoTracks objectAtIndex:0];
                AVAssetReaderTrackOutput* trackOutput = [AVAssetReaderTrackOutput 
                    assetReaderTrackOutputWithTrack:videoTrack 
                    outputSettings:nil];
                [newReader addOutput:trackOutput];
                ctx->reader = (__bridge_retained void*)newReader;
                ctx->track_output = (__bridge_retained void*)trackOutput;
            }
            reader = (__bridge AVAssetReader*)ctx->reader;
            if ([reader startReading]) { ctx->reader_started = 1; return 0; }
            NSLog(@"[shim] startReading failed: %@", reader.error);
            return -2;
        } @catch (NSException* e) {
            log_exception(__func__, e);
            return -1000;
        }
    }
}

double avfoundation_peek_first_sample_pts(AVFoundationContext* ctx) {
    @autoreleasepool {
        @try {
            if (!ctx || !ctx->track_output) return -1.0;
            AVAssetReaderTrackOutput* trackOutput = (__bridge AVAssetReaderTrackOutput*)ctx->track_output;
            CMSampleBufferRef sb = [trackOutput copyNextSampleBuffer];
            if (!sb) return -2.0;
            CMTime pts = CMSampleBufferGetPresentationTimeStamp(sb);
            double sec = CMTimeGetSeconds(pts);
            CFRelease(sb);
            return sec;
        } @catch (NSException* e) {
            log_exception(__func__, e);
            return -1000.0;
        }
    }
}

// VT wrapper functions
OSStatus avf_vt_create_session(CMFormatDescriptionRef fmt,
                               CFDictionaryRef dest_attrs,
                               VTDecompressionOutputCallback cb,
                               void *refcon,
                               VTDecompressionSessionRef *out_sess) {
  @autoreleasepool {
    @try {
      VTDecompressionOutputCallbackRecord rec = { .decompressionOutputCallback = cb,
                                                  .decompressionOutputRefCon  = refcon };
      return VTDecompressionSessionCreate(kCFAllocatorDefault,
                                          fmt,
                                          /*decoderSpecification*/ NULL,
                                          dest_attrs,
                                          &rec,
                                          out_sess);
    } @catch (NSException* e) {
      log_exception(__func__, e);
      return -10000; // custom OSStatus to indicate exception
    }
  }
}

OSStatus avf_vt_decode_frame(VTDecompressionSessionRef sess, CMSampleBufferRef sb) {
  @autoreleasepool {
    @try {
      return VTDecompressionSessionDecodeFrame(sess,
                                               sb,
                                               kVTDecodeFrame_EnableAsynchronousDecompression,
                                               sb, /* sourceFrameRefcon (we don't use it) */
                                               NULL);
    } @catch (NSException* e) {
      log_exception(__func__, e);
      return -10001;
    }
  }
}

void avf_vt_wait_async(VTDecompressionSessionRef sess) {
  @autoreleasepool {
    @try { VTDecompressionSessionWaitForAsynchronousFrames(sess); }
    @catch (NSException* e) { log_exception(__func__, e); }
  }
}

void avf_vt_invalidate(VTDecompressionSessionRef sess) {
  @autoreleasepool {
    @try { VTDecompressionSessionInvalidate(sess); }
    @catch (NSException* e) { log_exception(__func__, e); }
  }
}

// Create VT session with IOSurface destination attributes for zero-copy
OSStatus avf_vt_create_session_iosurface(CMFormatDescriptionRef fmt,
                                          VTDecompressionOutputCallback cb,
                                          void *refcon,
                                          VTDecompressionSessionRef *out_sess) {
  @autoreleasepool {
    @try {
      // Get video dimensions from format description
      CMVideoDimensions dimensions = CMVideoFormatDescriptionGetDimensions(fmt);
      int width = dimensions.width;
      int height = dimensions.height;
      
      // Create IOSurface-backed destination attributes
      NSDictionary* attrs = @{
        (NSString*)kCVPixelBufferPixelFormatTypeKey : @(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange),
        (NSString*)kCVPixelBufferWidthKey : @(width),
        (NSString*)kCVPixelBufferHeightKey : @(height),
        (NSString*)kCVPixelBufferIOSurfacePropertiesKey : @{},
        (NSString*)kCVPixelBufferMetalCompatibilityKey : @(YES)
      };
      
      VTDecompressionOutputCallbackRecord rec = { 
        .decompressionOutputCallback = cb,
        .decompressionOutputRefCon = refcon 
      };
      
      return VTDecompressionSessionCreate(kCFAllocatorDefault,
                                          fmt,
                                          /*decoderSpecification*/ NULL,
                                          (__bridge CFDictionaryRef)attrs,
                                          &rec,
                                          out_sess);
    } @catch (NSException* e) {
      log_exception(__func__, e);
      return -10000;
    }
  }
}

// Get IOSurface from CVPixelBuffer
IOSurfaceRef avf_cvpixelbuffer_get_iosurface(void* pixel_buffer) {
  @autoreleasepool {
    @try {
      if (!pixel_buffer) {
        return NULL;
      }
      
      CVPixelBufferRef cvPixelBuffer = (CVPixelBufferRef)pixel_buffer;
      IOSurfaceRef surface = CVPixelBufferGetIOSurface(cvPixelBuffer);
      
      if (surface) {
        // Retain the IOSurface before returning
        CFRetain(surface);
        // Throttle IOSurface logs to at most 1/sec when enabled
        static uint64_t last_log_ns = 0;
        uint64_t now = mach_absolute_time();
        static mach_timebase_info_data_t tb; if (!tb.denom) mach_timebase_info(&tb);
        uint64_t now_ns = now * tb.numer / tb.denom;
        if (avf_debug_iosurface && (now_ns - last_log_ns) >= 1000000000ULL) {
          AVF_LOG_IOS(@"Retrieved IOSurface from CVPixelBuffer: %dx%d",
                      (int)IOSurfaceGetWidth(surface),
                      (int)IOSurfaceGetHeight(surface));
          last_log_ns = now_ns;
        }
      }
      
      return surface;
    } @catch (NSException* e) {
      log_exception(__func__, e);
      return NULL;
    }
  }
}

// Create IOSurface destination attributes
const void* avf_create_iosurface_destination_attributes(int width, int height) {
  @autoreleasepool {
    @try {
      NSDictionary* attrs = @{
        (NSString*)kCVPixelBufferPixelFormatTypeKey : @(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange),
        (NSString*)kCVPixelBufferWidthKey : @(width),
        (NSString*)kCVPixelBufferHeightKey : @(height),
        (NSString*)kCVPixelBufferIOSurfacePropertiesKey : @{},
        (NSString*)kCVPixelBufferMetalCompatibilityKey : @(YES)
      };
      
      NSLog(@"Created IOSurface destination attributes: %dx%d", width, height);
      return (__bridge_retained CFDictionaryRef)attrs;
    } @catch (NSException* e) {
      log_exception(__func__, e);
      return NULL;
    }
  }
}

// --- IOSurface plane helpers (used by wgpu_integration.rs) ---
void avf_iosurface_lock_readonly(IOSurfaceRef surface) {
  @autoreleasepool {
    @try {
      IOSurfaceLock(surface, kIOSurfaceLockReadOnly, NULL);
    } @catch (NSException *e) {
      NSLog(@"[shim] IOSurfaceLock EXC: %@ — %@", e.name, e.reason);
    }
  }
}

void avf_iosurface_unlock(IOSurfaceRef surface) {
  @autoreleasepool {
    @try {
      IOSurfaceUnlock(surface, kIOSurfaceLockReadOnly, NULL);
    } @catch (NSException *e) {
      NSLog(@"[shim] IOSurfaceUnlock EXC: %@ — %@", e.name, e.reason);
    }
  }
}

size_t avf_iosurface_width_of_plane(IOSurfaceRef surface, size_t plane) {
  return IOSurfaceGetWidthOfPlane(surface, plane);
}

size_t avf_iosurface_height_of_plane(IOSurfaceRef surface, size_t plane) {
  return IOSurfaceGetHeightOfPlane(surface, plane);
}

size_t avf_iosurface_bytes_per_row_of_plane(IOSurfaceRef surface, size_t plane) {
  return IOSurfaceGetBytesPerRowOfPlane(surface, plane);
}

void* avf_iosurface_base_address_of_plane(IOSurfaceRef surface, size_t plane) {
  return IOSurfaceGetBaseAddressOfPlane(surface, plane);
}
