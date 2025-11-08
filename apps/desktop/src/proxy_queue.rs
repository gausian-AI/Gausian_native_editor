use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::{self, Receiver, Sender};
use project::{AssetMediaDetails, AssetRow, ProjectDb, ProxyJobInsert};

use crate::media_info::{probe_media_info, HardwareCaps, MediaInfo};
use crate::playback_selector::register_proxy;
use crate::proxy_pipeline::{
    run_proxy_pipeline, ProxyPipelineConfig, ProxyPipelineStatus, ProxyPreset,
};
use uuid::Uuid;

const TARGET_PROXY_HEIGHT: u32 = 960;
const MAX_PROXY_BYTES: u64 = 100 * 1024 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ProxyEnqueueRequest {
    pub project_id: String,
    pub asset_id: String,
    pub reason: ProxyReason,
    pub force: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyReason {
    Import,
    Timeline,
    PlaybackLag,
    Manual,
    Mode,
}

impl ProxyReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ProxyReason::Import => "import",
            ProxyReason::Timeline => "timeline",
            ProxyReason::PlaybackLag => "playback_lag",
            ProxyReason::Manual => "manual",
            ProxyReason::Mode => "mode",
        }
    }
}

#[derive(Debug, Clone)]
pub enum ProxyStatus {
    Pending,
    Running { progress: f32 },
    Completed { proxy_path: PathBuf },
    Failed { message: String },
}

#[derive(Debug, Clone)]
pub struct ProxyEvent {
    pub job_id: String,
    pub asset_id: String,
    pub status: ProxyStatus,
}

#[derive(Clone)]
pub struct ProxyQueue {
    tx_cmd: Sender<Command>,
}

impl ProxyQueue {
    pub fn start(
        db_path: PathBuf,
        hardware_caps: Arc<HardwareCaps>,
    ) -> (Self, Receiver<ProxyEvent>) {
        let (tx_cmd, rx_cmd) = crossbeam_channel::unbounded::<Command>();
        let (tx_event, rx_event) = crossbeam_channel::unbounded::<ProxyEvent>();

        let worker = Worker::new(db_path, hardware_caps, rx_cmd, tx_event);
        thread::spawn(move || worker.run());

        (Self { tx_cmd }, rx_event)
    }

    pub fn enqueue(&self, req: ProxyEnqueueRequest) {
        let _ = self.tx_cmd.send(Command::Enqueue(req));
    }
}

enum Command {
    Enqueue(ProxyEnqueueRequest),
    Shutdown,
}

struct Worker {
    db_path: PathBuf,
    hardware_caps: Arc<HardwareCaps>,
    rx_cmd: Receiver<Command>,
    tx_event: Sender<ProxyEvent>,
    running: HashSet<String>,
    inflight: HashMap<String, thread::JoinHandle<()>>,
    completion_rx: Receiver<Completion>,
    completion_tx: Sender<Completion>,
    last_pending_scan: Instant,
    concurrency: usize,
}

struct Completion {
    pub job_id: String,
}

impl Worker {
    fn new(
        db_path: PathBuf,
        hardware_caps: Arc<HardwareCaps>,
        rx_cmd: Receiver<Command>,
        tx_event: Sender<ProxyEvent>,
    ) -> Self {
        let (completion_tx, completion_rx) = crossbeam_channel::unbounded();
        let logical = hardware_caps.logical_cores.max(1);
        let concurrency = (logical / 2).max(1);
        Self {
            db_path,
            hardware_caps,
            rx_cmd,
            tx_event,
            running: HashSet::new(),
            inflight: HashMap::new(),
            completion_rx,
            completion_tx,
            last_pending_scan: Instant::now() - Duration::from_secs(5),
            concurrency,
        }
    }

    fn run(mut self) {
        let mut pending_cache: Vec<project::ProxyJobRow> = Vec::new();

        loop {
            // Drain completions from workers
            while let Ok(done) = self.completion_rx.try_recv() {
                self.running.remove(&done.job_id);
                if let Some(handle) = self.inflight.remove(&done.job_id) {
                    let _ = handle.join();
                }
            }

            // Handle commands without blocking the loop
            match self.rx_cmd.try_recv() {
                Ok(Command::Enqueue(req)) => {
                    if let Err(err) = self.prepare_job(req) {
                        tracing::warn!("proxy enqueue failed: {err:?}");
                    }
                    // Force next scan to pick up fresh job immediately.
                    self.last_pending_scan = Instant::now() - Duration::from_secs(10);
                }
                Ok(Command::Shutdown) | Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    break;
                }
                Err(crossbeam_channel::TryRecvError::Empty) => {}
            }

            // Launch new jobs if capacity available
            if self.running.len() < self.concurrency {
                if self.last_pending_scan.elapsed() > Duration::from_millis(250) {
                    pending_cache = self.load_pending_jobs().unwrap_or_else(|_| Vec::new());
                    self.last_pending_scan = Instant::now();
                }

                if let Some(next_job) = pending_cache
                    .iter()
                    .find(|job| !self.running.contains(&job.id))
                    .cloned()
                {
                    if let Err(err) = self.start_job(next_job.clone()) {
                        tracing::error!("proxy job start error: {err:?}");
                        if let Err(db_err) = self.mark_job_failed(
                            &next_job.id,
                            &next_job.asset_id,
                            &format!("{err:?}"),
                        ) {
                            tracing::error!("failed to mark proxy job failed: {db_err:?}");
                        }
                    } else {
                        pending_cache.retain(|job| job.id != next_job.id);
                    }
                }
            }

            thread::sleep(Duration::from_millis(50));
        }
    }

    fn prepare_job(&self, req: ProxyEnqueueRequest) -> Result<()> {
        let db = ProjectDb::open_or_create(&self.db_path)?;
        let asset = db
            .get_asset(&req.asset_id)
            .context("load asset for proxy enqueue")?;

        if !asset.kind.eq_ignore_ascii_case("video") {
            return Ok(()); // images/audio skip
        }

        if asset.is_proxy_ready && !req.force {
            return Ok(());
        }

        if !req.force {
            if let Some(existing) = db.find_proxy_job_for_asset(&asset.id)? {
                if matches!(existing.status.as_str(), "pending" | "running") {
                    return Ok(());
                }
            }
        }

        let media_info =
            probe_media_info(Path::new(&asset.src_abs)).context("probe media for proxy job")?;
        let (target_w, target_h) = compute_target_dimensions(&media_info);
        let preset = choose_preset(&self.hardware_caps);
        let preset_name = preset_name(preset);
        let ext = match preset {
            ProxyPreset::MacProRes => "mov",
            ProxyPreset::DnxhrLb => "mov",
        };

        let proxy_root = resolve_proxy_root(&db, &req.project_id, &asset)?.join(preset_name);
        let original = Path::new(&asset.src_abs);
        let stem = original
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("clip");
        let proxy_filename = format!("{stem}__pxy_{target_h}p.{ext}");
        let proxy_path = proxy_root.join(proxy_filename);

        let job_id = Uuid::new_v4().to_string();

        db.insert_proxy_job(&ProxyJobInsert {
            id: &job_id,
            project_id: &req.project_id,
            asset_id: &asset.id,
            original_path: original,
            proxy_path: proxy_path.as_path(),
            preset: preset_name,
            reason: Some(req.reason.as_str()),
            width: Some(target_w as i64),
            height: Some(target_h as i64),
            bitrate_kbps: None,
        })?;

        let codec_opt = media_info.codec.as_deref();
        let mut details = AssetMediaDetails::default();
        details.duration_seconds = media_info.duration_seconds;
        details.codec = codec_opt;
        details.bitrate_mbps = media_info.bitrate_mbps;
        details.proxy_path = Some(proxy_path.as_path());
        details.is_proxy_ready = Some(false);
        details.bit_depth = media_info.bit_depth;
        details.is_hdr = Some(media_info.is_hdr);
        details.is_variable_framerate = Some(media_info.is_variable_framerate);
        db.update_asset_media_details(&asset.id, &details)?;

        let _ = self.tx_event.send(ProxyEvent {
            job_id,
            asset_id: asset.id,
            status: ProxyStatus::Pending,
        });

        Ok(())
    }

    fn load_pending_jobs(&self) -> Result<Vec<project::ProxyJobRow>> {
        let db = ProjectDb::open_or_create(&self.db_path)?;
        db.list_proxy_jobs_by_status("pending")
    }

    fn start_job(&mut self, job: project::ProxyJobRow) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        let db = ProjectDb::open_or_create(&self.db_path)?;
        db.update_proxy_job_status(&job.id, "running", Some(0.0), None, Some(now), None)?;

        let _ = self.tx_event.send(ProxyEvent {
            job_id: job.id.clone(),
            asset_id: job.asset_id.clone(),
            status: ProxyStatus::Running { progress: 0.0 },
        });

        self.running.insert(job.id.clone());

        let completion_tx = self.completion_tx.clone();
        let tx_event = self.tx_event.clone();
        let db_path = self.db_path.clone();
        let hardware_caps = self.hardware_caps.clone();
        let job_for_thread = job.clone();

        let handle = thread::spawn(move || {
            let result = execute_job(&db_path, &hardware_caps, &job_for_thread, tx_event.clone());
            match result {
                Ok(proxy_path) => {
                    let _ = tx_event.send(ProxyEvent {
                        job_id: job_for_thread.id.clone(),
                        asset_id: job_for_thread.asset_id.clone(),
                        status: ProxyStatus::Completed { proxy_path },
                    });
                }
                Err(err) => {
                    let _ = tx_event.send(ProxyEvent {
                        job_id: job_for_thread.id.clone(),
                        asset_id: job_for_thread.asset_id.clone(),
                        status: ProxyStatus::Failed {
                            message: err.to_string(),
                        },
                    });
                }
            }
            let _ = completion_tx.send(Completion {
                job_id: job_for_thread.id.clone(),
            });
        });

        self.inflight.insert(job.id.clone(), handle);
        Ok(())
    }

    fn mark_job_failed(&self, job_id: &str, asset_id: &str, message: &str) -> Result<()> {
        let db = ProjectDb::open_or_create(&self.db_path)?;
        db.update_proxy_job_status(
            job_id,
            "failed",
            None,
            Some(message),
            None,
            Some(chrono::Utc::now().timestamp()),
        )?;
        let _ = self.tx_event.send(ProxyEvent {
            job_id: job_id.to_string(),
            asset_id: asset_id.to_string(),
            status: ProxyStatus::Failed {
                message: message.to_string(),
            },
        });
        Ok(())
    }
}

fn execute_job(
    db_path: &Path,
    hardware_caps: &HardwareCaps,
    job: &project::ProxyJobRow,
    tx_event: Sender<ProxyEvent>,
) -> Result<PathBuf> {
    let source = PathBuf::from(&job.original_path);
    let destination = PathBuf::from(&job.proxy_path);

    let preset = match job.preset.as_str() {
        "mac_prores" => ProxyPreset::MacProRes,
        "dnxhr_lb" => ProxyPreset::DnxhrLb,
        _ => ProxyPreset::DnxhrLb,
    };

    let config = ProxyPipelineConfig {
        source: source.clone(),
        destination: destination.clone(),
        preset,
        width: job.width.unwrap_or(TARGET_PROXY_HEIGHT as i64) as u32,
        height: job.height.unwrap_or(TARGET_PROXY_HEIGHT as i64) as u32,
        bitrate_kbps: job.bitrate_kbps.unwrap_or(0) as u32,
        decoder: select_decoder(hardware_caps),
    };

    let asset_duration = {
        let db = ProjectDb::open_or_create(db_path)?;
        db.get_asset(&job.asset_id)
            .ok()
            .and_then(|asset| asset.duration_seconds)
            .filter(|d| *d > 0.0)
    };
    let expected_bytes = asset_duration.and_then(|duration| {
        job.bitrate_kbps.map(|kbps| {
            let bytes_per_sec = (kbps as f64 * 1000.0) / 8.0;
            (bytes_per_sec * duration).max(1.0)
        })
    });
    let mut watcher = ProxyProgressWatcher::start(
        destination.clone(),
        expected_bytes,
        job.id.clone(),
        job.asset_id.clone(),
        tx_event.clone(),
        db_path.to_path_buf(),
    );

    let (status_tx, status_rx) = mpsc::channel();
    let pipeline_handle = run_proxy_pipeline(config, status_tx)?;

    let mut pipeline_error: Option<anyhow::Error> = None;
    loop {
        match status_rx.recv() {
            Ok(ProxyPipelineStatus::Progress(message)) => {
                tracing::debug!(%message, "proxy pipeline progress");
            }
            Ok(ProxyPipelineStatus::Completed) => {
                break;
            }
            Ok(ProxyPipelineStatus::Error(message)) => {
                tracing::error!(%message, "proxy pipeline error");
                pipeline_error = Some(anyhow!(message));
                break;
            }
            Err(recv_err) => {
                pipeline_error = Some(anyhow!(format!(
                    "proxy pipeline status channel closed unexpectedly: {recv_err}"
                )));
                break;
            }
        }
    }

    watcher.stop();

    let pipeline_join_result = pipeline_handle
        .join()
        .map_err(|_| anyhow!("proxy pipeline thread panicked"))?;

    if let Err(err) = pipeline_join_result {
        return Err(err);
    }

    if let Some(err) = pipeline_error {
        return Err(err);
    }

    let db = ProjectDb::open_or_create(db_path)?;
    let now = chrono::Utc::now().timestamp();
    db.update_proxy_job_status(&job.id, "done", Some(1.0), None, None, Some(now))?;

    let mut details = AssetMediaDetails::default();
    details.proxy_path = Some(destination.as_path());
    details.is_proxy_ready = Some(true);
    db.update_asset_media_details(&job.asset_id, &details)?;

    register_proxy(source.clone(), destination.clone());

    prune_proxy_storage(&db, &job.project_id, MAX_PROXY_BYTES)?;

    Ok(destination)
}

struct ProxyProgressWatcher {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ProxyProgressWatcher {
    fn start(
        destination: PathBuf,
        expected_bytes: Option<f64>,
        job_id: String,
        asset_id: String,
        tx_event: Sender<ProxyEvent>,
        db_path: PathBuf,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let handle = thread::spawn(move || {
            let start = Instant::now();
            let mut last_progress = -1.0f32;
            let fallback_window = Duration::from_secs(30);
            while !stop_flag.load(Ordering::Relaxed) {
                let mut progress = None;
                if let Some(expected) = expected_bytes {
                    if expected > 0.0 {
                        if let Ok(meta) = std::fs::metadata(&destination) {
                            let pct = (meta.len() as f64 / expected).clamp(0.0, 0.99) as f32;
                            progress = Some(pct);
                        }
                    }
                }
                if progress.is_none() {
                    let elapsed = start.elapsed();
                    let pct = (elapsed.as_secs_f32() / fallback_window.as_secs_f32()).min(0.9);
                    progress = Some(pct);
                }
                if let Some(mut pct) = progress {
                    pct = pct.clamp(0.01, 0.99);
                    if last_progress < 0.0 || (pct - last_progress).abs() >= 0.05 {
                        let _ = tx_event.send(ProxyEvent {
                            job_id: job_id.clone(),
                            asset_id: asset_id.clone(),
                            status: ProxyStatus::Running { progress: pct },
                        });
                        if let Ok(db) = ProjectDb::open_or_create(&db_path) {
                            let _ = db.update_proxy_job_status(
                                &job_id,
                                "running",
                                Some(pct as f64),
                                None,
                                None,
                                None,
                            );
                        }
                        last_progress = pct;
                    }
                }
                thread::sleep(Duration::from_millis(700));
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn select_decoder(hardware_caps: &HardwareCaps) -> Option<String> {
    hardware_caps
        .decoder_elements
        .iter()
        .find(|name| {
            name.contains("dec")
                && (name.contains("265")
                    || name.contains("264")
                    || name.contains("hevc")
                    || name.contains("h264"))
        })
        .cloned()
}

fn compute_target_dimensions(info: &MediaInfo) -> (u32, u32) {
    let width = info.width.unwrap_or(1920).max(16);
    let height = info.height.unwrap_or(1080).max(16);
    let longest = width.max(height);

    if longest <= TARGET_PROXY_HEIGHT {
        (ensure_even(width), ensure_even(height))
    } else {
        let scale = TARGET_PROXY_HEIGHT as f64 / longest as f64;
        let mut w = (width as f64 * scale).round() as u32;
        let mut h = (height as f64 * scale).round() as u32;
        if w == 0 {
            w = 16;
        }
        if h == 0 {
            h = 16;
        }
        (ensure_even(w), ensure_even(h))
    }
}

fn prune_proxy_storage(db: &ProjectDb, project_id: &str, limit_bytes: u64) -> Result<()> {
    let assets = db.list_assets(project_id)?;
    let mut total_size: u64 = 0;
    let mut tracked: Vec<(PathBuf, u64, SystemTime, String)> = Vec::new();

    for asset in &assets {
        let Some(proxy_str) = asset.proxy_path.as_ref() else {
            continue;
        };
        let proxy_path = PathBuf::from(proxy_str);
        let original_exists = Path::new(&asset.src_abs).exists();
        match std::fs::metadata(&proxy_path) {
            Ok(meta) => {
                if !original_exists {
                    let _ = std::fs::remove_file(&proxy_path);
                    let mut details = AssetMediaDetails::default();
                    details.proxy_path = None;
                    details.is_proxy_ready = Some(false);
                    let _ = db.update_asset_media_details(&asset.id, &details);
                } else {
                    let size = meta.len();
                    let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                    total_size = total_size.saturating_add(size);
                    tracked.push((proxy_path.clone(), size, modified, asset.id.clone()));
                }
            }
            Err(_) => {
                let mut details = AssetMediaDetails::default();
                details.proxy_path = None;
                details.is_proxy_ready = Some(false);
                let _ = db.update_asset_media_details(&asset.id, &details);
            }
        }
    }

    if total_size <= limit_bytes {
        return Ok(());
    }

    tracked.sort_by_key(|(_, _, modified, _)| *modified);
    for (path, size, _, asset_id) in tracked {
        if total_size <= limit_bytes {
            break;
        }
        match std::fs::remove_file(&path) {
            Ok(_) => {
                total_size = total_size.saturating_sub(size);
                let mut details = AssetMediaDetails::default();
                details.proxy_path = None;
                details.is_proxy_ready = Some(false);
                let _ = db.update_asset_media_details(&asset_id, &details);
            }
            Err(err) => {
                tracing::warn!(path = %path.display(), "failed to remove proxy during cleanup: {err}");
            }
        }
    }

    Ok(())
}

fn ensure_even(v: u32) -> u32 {
    if v % 2 == 0 {
        v.max(2)
    } else {
        (v + 1).max(2)
    }
}

fn choose_preset(hw: &HardwareCaps) -> ProxyPreset {
    if cfg!(target_os = "macos") && hw.prefers_prores_proxy {
        ProxyPreset::MacProRes
    } else {
        ProxyPreset::DnxhrLb
    }
}

fn preset_name(preset: ProxyPreset) -> &'static str {
    match preset {
        ProxyPreset::MacProRes => "mac_prores",
        ProxyPreset::DnxhrLb => "dnxhr_lb",
    }
}

fn resolve_proxy_root(db: &ProjectDb, project_id: &str, asset: &AssetRow) -> Result<PathBuf> {
    let default_base = default_project_base_path(project_id);
    let app_root = project::app_data_dir();

    if let Some(base) = db.get_project_base_path(project_id)? {
        let base_path = PathBuf::from(base);
        let inside_app_dir = base_path.starts_with(&app_root);
        match ensure_writable_directory(&base_path) {
            Ok(()) if inside_app_dir => {
                return Ok(base_path.join("media").join("proxy"));
            }
            Ok(()) => {
                tracing::info!(
                    project_id,
                    base = %base_path.display(),
                    default = %default_base.display(),
                    "project base path outside app data directory; switching to default location"
                );
            }
            Err(err) => {
                tracing::warn!(
                    project_id,
                    base = %base_path.display(),
                    "project base path unusable; attempting default location: {err:?}"
                );
            }
        }
    }

    match ensure_writable_directory(&default_base) {
        Ok(()) => {
            let _ = db.set_project_base_path(project_id, &default_base);
            tracing::info!(
                project_id,
                base = %default_base.display(),
                "using default project base path for proxies"
            );
            return Ok(default_base.join("media").join("proxy"));
        }
        Err(err) => {
            tracing::error!(
                project_id,
                base = %default_base.display(),
                "default project base path unusable: {err:?}"
            );
        }
    }

    let fallback_parent = Path::new(&asset.src_abs)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| project::app_data_dir().join("media"));
    ensure_writable_directory(&fallback_parent).with_context(|| {
        format!(
            "ensure proxy fallback directory {} is writable",
            fallback_parent.display()
        )
    })?;
    Ok(fallback_parent.join("proxy"))
}

fn ensure_writable_directory(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("create project directory hierarchy at {}", path.display()))?;

    let meta = std::fs::metadata(path)
        .with_context(|| format!("retrieve metadata for {}", path.display()))?;
    if !meta.is_dir() {
        return Err(anyhow!(
            "expected {} to be a directory but found something else",
            path.display()
        ));
    }

    let probe_path = path.join(".gausian_write_probe");
    let probe_result = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&probe_path);
    match probe_result {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe_path);
            Ok(())
        }
        Err(err) => Err(anyhow!(
            "failed to create write probe file {}: {err}",
            probe_path.display()
        )),
    }
}

fn default_project_base_path(project_id: &str) -> PathBuf {
    project::app_data_dir().join("projects").join(project_id)
}
