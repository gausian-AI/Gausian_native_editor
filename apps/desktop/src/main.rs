#![deny(clippy::disallowed_methods)]

use anyhow::{anyhow, Context, Result};
use eframe::egui_wgpu;
use eframe::{
    egui::{self, TextureHandle},
    NativeOptions,
};
use project::{AssetRow, ProjectDb};
extern crate jobs as jobs_crate;
extern crate timeline as timeline_crate;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, OnceLock,
};
use std::time::{Duration, Instant};
use timeline_crate::{
    ClipNode, CommandHistory, Fps, FrameRange, Item, ItemKind, NodeId, Sequence, TimelineCommand,
    TimelineError, TimelineNode, TimelineNodeKind, Track, TrackKind, TrackPlacement,
};
mod clock;
mod decode;
mod interaction;
mod timeline;
use clock::PlaybackClock;
use decode::{
    DecodeCmd, DecodeManager, EngineState, FramePayload, PlayState, VideoFrameOut, VideoProps,
};
use interaction::{DragMode, DragState};
mod audio_decode;
mod audio_engine;
mod cache;
mod comfyui;
mod embed_webview;
mod export;
mod gpu;
mod jobs;
mod media_info;
mod playback_selector;
mod preview;
mod prompt_normalize;
mod proxy_pipeline;
mod proxy_policy;
mod proxy_queue;
mod screenplay;
use audio_decode::decode_audio_to_buffer;
use audio_engine::{ActiveAudioClip, AudioBuffer, AudioEngine};
pub use export::{ExportCodec, ExportPreset, ExportProgress, ExportUiState};
use jobs_crate::{JobEvent, JobStatus};
use native_decoder::{
    create_decoder, is_native_decoding_available, DecoderConfig, VideoFrame,
    YuvPixFmt as NativeYuvPixFmt,
};
// use preview::visual_source_at;
use preview::PreviewState;
use std::collections::HashMap;
// use std::collections::VecDeque;
// use std::hash::Hash;
use crossbeam_channel::{unbounded, Receiver, Sender};
use walkdir::WalkDir;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloudUpdateSrc {
    Ws,
    Jobs,
    Status,
}

enum ModalEvent {
    Log(String),
    JobQueued(String),
    // (job_id, unique filename prefix)
    JobQueuedWithPrefix(String, String),
    // (job_id, [(filename, url)])
    Recent(Vec<(String, Vec<(String, String)>)>),
    CloudStatus {
        pending: usize,
        running: usize,
    },
    CloudProgress {
        job_id: String,
        progress: f32,
        current: u32,
        total: u32,
        node_id: Option<String>,
    },
    CloudSource {
        job_id: String,
        source: CloudUpdateSrc,
    },
    JobImporting(String),
    JobImported(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CloudTarget {
    Prompt,
    Workflow,
}

static PRESENT_SIZE_MISMATCH_LOGGED: OnceLock<AtomicBool> = OnceLock::new();

use tracing_subscriber::EnvFilter;

fn main() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .try_init();
    match native_decoder::describe_platform_decoder() {
        Ok(desc) => println!("Native decoder selected: {desc}"),
        Err(err) => println!("Native decoder selection failed: {err}"),
    }
    // Ensure DB exists before UI
    let data_dir = project::app_data_dir();
    std::fs::create_dir_all(&data_dir).expect("create data dir");
    let db_path = data_dir.join("app.db");
    let db = ProjectDb::open_or_create(&db_path).expect("open db");

    let options = NativeOptions {
        viewport: egui::ViewportBuilder::default(),
        ..NativeOptions::default()
    };
    let _ = eframe::run_native(
        "Gausian Native Editor",
        options,
        Box::new(move |_cc| Ok(Box::new(App::new(db)))),
    );
}

fn nearest_common_ancestor(paths: &[PathBuf]) -> Option<PathBuf> {
    if paths.is_empty() {
        return None;
    }
    // Normalize each path to a directory (if it's a file, use its parent).
    let to_dir = |p: &PathBuf| -> PathBuf {
        match std::fs::metadata(p) {
            Ok(md) => {
                if md.is_file() {
                    p.parent()
                        .map(|pp| pp.to_path_buf())
                        .unwrap_or_else(|| p.clone())
                } else {
                    p.clone()
                }
            }
            Err(_) => p
                .parent()
                .map(|pp| pp.to_path_buf())
                .unwrap_or_else(|| p.clone()),
        }
    };
    let mut it = paths.iter();
    let first = it.next()?;
    let mut acc = first
        .ancestors()
        .map(|p| to_dir(&p.to_path_buf()))
        .collect::<Vec<_>>();
    for p in it {
        let set = p
            .ancestors()
            .map(|a| to_dir(&a.to_path_buf()))
            .collect::<Vec<_>>();
        acc.retain(|cand| set.contains(cand));
        if acc.is_empty() {
            break;
        }
    }
    acc.first().cloned()
}

#[derive(Clone, Debug)]
struct VisualSource {
    path: String,
    is_image: bool,
}

#[derive(Clone, Debug)]
struct AudioPeaks {
    peaks: Vec<(f32, f32)>, // (min, max) in [-1,1]
    duration_sec: f32,
    channels: u16,
    sample_rate: u32,
}

#[derive(Default)]
struct AudioCache {
    map: std::collections::HashMap<std::path::PathBuf, std::sync::Arc<AudioPeaks>>,
}

#[derive(Default)]
struct AudioBufferCache {
    map: HashMap<PathBuf, Arc<AudioBuffer>>,
}

impl AudioBufferCache {
    fn get_or_load(&mut self, path: &Path) -> anyhow::Result<Arc<AudioBuffer>> {
        if let Some(buf) = self.map.get(path) {
            return Ok(buf.clone());
        }
        let decoded = decode_audio_to_buffer(path)?;
        let arc = Arc::new(decoded);
        self.map.insert(path.to_path_buf(), arc.clone());
        Ok(arc)
    }
}

include!("app.rs");
