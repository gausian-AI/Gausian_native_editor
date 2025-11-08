use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context, Result};
use glib::{self, prelude::*};
use gstreamer as gst;
use gstreamer::prelude::*;
use parking_lot::{Condvar, Mutex};
use sha1::{Digest, Sha1};
use tracing::{debug, error, info, warn};

use super::{
    job::{CacheEvent, CacheJobId, CacheJobSpec, JobStatus, PreferredCodec},
    pipeline,
};

struct State {
    next_id: u64,
    jobs: HashMap<CacheJobId, JobRecord>,
    dedup: HashMap<String, CacheJobId>,
}

struct JobRecord {
    spec: CacheJobSpec,
    status: JobStatus,
    cancel: Arc<AtomicBool>,
    output_path: PathBuf,
    tmp_path: PathBuf,
    dedup_key: String,
    started_at: Option<Instant>,
}

struct JobSemaphore {
    max: usize,
    active: Mutex<usize>,
    condvar: Condvar,
}

impl JobSemaphore {
    fn new(max: usize) -> Self {
        Self {
            max,
            active: Mutex::new(0),
            condvar: Condvar::new(),
        }
    }

    fn acquire(&self) -> JobPermit<'_> {
        let mut active = self.active.lock();
        while *active >= self.max {
            self.condvar.wait(&mut active);
        }
        *active += 1;
        JobPermit { sem: self }
    }

    fn release(&self) {
        let mut active = self.active.lock();
        if *active > 0 {
            *active -= 1;
        }
        self.condvar.notify_one();
    }
}

struct JobPermit<'a> {
    sem: &'a JobSemaphore,
}

impl Drop for JobPermit<'_> {
    fn drop(&mut self) {
        self.sem.release();
    }
}

enum JobFinish {
    Completed(PathBuf),
    Failed(String),
    Canceled,
}

#[derive(Clone)]
pub struct CacheManager {
    inner: Arc<Mutex<State>>,
    events_tx: Sender<CacheEvent>,
    subscribers: Arc<Mutex<Vec<Sender<CacheEvent>>>>,
    optimized_root: PathBuf,
    permits: Arc<JobSemaphore>,
    max_concurrency: usize,
}

impl CacheManager {
    pub fn new(root: PathBuf, max: usize) -> Result<Self> {
        if max == 0 {
            return Err(anyhow!("CacheManager requires max concurrency > 0"));
        }

        fs::create_dir_all(&root)
            .with_context(|| format!("create optimized cache root {}", root.display()))?;

        let (events_tx, events_rx) = mpsc::channel();
        let subscribers = Arc::new(Mutex::new(Vec::new()));
        spawn_event_dispatch(events_rx, Arc::clone(&subscribers));

        let manager = Self {
            inner: Arc::new(Mutex::new(State {
                next_id: 1,
                jobs: HashMap::new(),
                dedup: HashMap::new(),
            })),
            events_tx,
            subscribers,
            optimized_root: root,
            permits: Arc::new(JobSemaphore::new(max)),
            max_concurrency: max,
        };

        info!(
            target = "cache",
            root = %manager.optimized_root.display(),
            max_concurrency = manager.max_concurrency,
            "cache manager initialized"
        );

        Ok(manager)
    }

    pub fn submit_cache_job(&self, spec: CacheJobSpec) -> CacheJobId {
        let prepared = match prepare_job(&self.optimized_root, spec) {
            Ok(prepared) => prepared,
            Err(err) => {
                error!(target = "cache", error = %err, "failed to prepare cache job");
                return self.record_failed_submission(err.to_string());
            }
        };

        let mut inner = self.inner.lock();
        if let Some(existing_id) = inner.dedup.get(&prepared.dedup_key).copied() {
            if let Some(record) = inner.jobs.get(&existing_id) {
                match &record.status {
                    JobStatus::Completed(path) if path.exists() => {
                        debug!(
                            target = "cache",
                            id = existing_id.0,
                            "deduplicated to completed optimized media"
                        );
                        self.emit_status(existing_id, JobStatus::Completed(path.clone()));
                        return existing_id;
                    }
                    JobStatus::Queued | JobStatus::InProgress(_) => {
                        debug!(
                            target = "cache",
                            id = existing_id.0,
                            "coalesced optimized media job"
                        );
                        return existing_id;
                    }
                    JobStatus::Failed(_) | JobStatus::Canceled | JobStatus::Completed(_) => {
                        // Output missing or job failed previously; fall through to new submission.
                        inner.dedup.remove(&prepared.dedup_key);
                    }
                }
            }
        }

        let job_id = CacheJobId(inner.next_id);
        inner.next_id += 1;

        let cancel = Arc::new(AtomicBool::new(false));
        let record = JobRecord {
            spec: prepared.spec.clone(),
            status: JobStatus::Queued,
            cancel: Arc::clone(&cancel),
            output_path: prepared.output_path.clone(),
            tmp_path: prepared.tmp_path.clone(),
            dedup_key: prepared.dedup_key.clone(),
            started_at: None,
        };
        inner.dedup.insert(prepared.dedup_key, job_id);
        inner.jobs.insert(job_id, record);
        drop(inner);

        info!(
            target = "cache",
            id = job_id.0,
            source = %prepared.spec.source_path.display(),
            output = %prepared.output_path.display(),
            "cache job queued"
        );
        self.emit_status(job_id, JobStatus::Queued);

        self.spawn_worker(
            job_id,
            prepared.spec,
            prepared.output_path,
            prepared.tmp_path,
            cancel,
        );

        job_id
    }

    pub fn cancel(&self, id: CacheJobId) {
        let inner = self.inner.lock();
        if let Some(job) = inner.jobs.get(&id) {
            job.cancel.store(true, Ordering::Relaxed);
            debug!(target = "cache", id = id.0, "cancel requested");
        }
    }

    pub fn status(&self, id: CacheJobId) -> Option<JobStatus> {
        let inner = self.inner.lock();
        inner.jobs.get(&id).map(|job| job.status.clone())
    }

    pub fn cached_output_path(&self, source: &Path, codec: PreferredCodec) -> Option<PathBuf> {
        let mut spec = CacheJobSpec {
            source_path: source.to_path_buf(),
            force_container_mov: true,
            preferred_codec: codec,
            source_codec: None,
        };
        spec.source_path = spec.source_path.canonicalize().ok()?;
        let (output_path, _, _) = compute_cache_paths(&self.optimized_root, &spec).ok()?;
        if output_path.exists() {
            Some(output_path)
        } else {
            None
        }
    }

    pub fn subscribe(&self) -> Receiver<CacheEvent> {
        let (tx, rx) = mpsc::channel();
        self.subscribers.lock().push(tx);
        rx
    }

    fn spawn_worker(
        &self,
        id: CacheJobId,
        spec: CacheJobSpec,
        output_path: PathBuf,
        tmp_path: PathBuf,
        cancel: Arc<AtomicBool>,
    ) {
        let inner = Arc::clone(&self.inner);
        let events_tx = self.events_tx.clone();
        let permits = Arc::clone(&self.permits);

        thread::Builder::new()
            .name(format!("cache-worker-{}", id.0))
            .spawn(move || {
                if let Err(err) = run_worker(
                    id,
                    spec,
                    output_path,
                    tmp_path,
                    cancel,
                    inner,
                    events_tx,
                    permits,
                ) {
                    error!(target = "cache", id = id.0, error = %err, "worker exited with error");
                }
            })
            .expect("failed to spawn cache worker thread");
    }

    fn emit_status(&self, id: CacheJobId, status: JobStatus) {
        if let Err(err) = self.events_tx.send(CacheEvent::StatusChanged {
            id,
            status: status.clone(),
        }) {
            warn!(target = "cache", error = %err, "failed to broadcast cache event");
        }
    }

    fn record_failed_submission(&self, message: String) -> CacheJobId {
        let mut inner = self.inner.lock();
        let job_id = CacheJobId(inner.next_id);
        inner.next_id += 1;

        inner.jobs.insert(
            job_id,
            JobRecord {
                spec: CacheJobSpec {
                    source_path: PathBuf::new(),
                    force_container_mov: true,
                    preferred_codec: PreferredCodec::ProRes422,
                    source_codec: None,
                },
                status: JobStatus::Failed(message.clone()),
                cancel: Arc::new(AtomicBool::new(false)),
                output_path: PathBuf::new(),
                tmp_path: PathBuf::new(),
                dedup_key: String::new(),
                started_at: None,
            },
        );
        drop(inner);

        let _ = self.events_tx.send(CacheEvent::StatusChanged {
            id: job_id,
            status: JobStatus::Failed(message),
        });
        job_id
    }
}

struct PreparedJob {
    spec: CacheJobSpec,
    output_path: PathBuf,
    tmp_path: PathBuf,
    dedup_key: String,
}

fn prepare_job(root: &Path, mut spec: CacheJobSpec) -> Result<PreparedJob> {
    spec.source_path = spec
        .source_path
        .canonicalize()
        .with_context(|| format!("canonicalize {}", spec.source_path.display()))?;
    let (output_path, tmp_path, dedup_key) = compute_cache_paths(root, &spec)?;
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create directory {}", parent.display()))?;
    }
    Ok(PreparedJob {
        spec,
        output_path,
        tmp_path,
        dedup_key,
    })
}

fn compute_cache_paths(root: &Path, spec: &CacheJobSpec) -> Result<(PathBuf, PathBuf, String)> {
    let metadata = fs::metadata(&spec.source_path)
        .with_context(|| format!("metadata {}", spec.source_path.display()))?;
    let modified = metadata
        .modified()
        .unwrap_or_else(|_| std::time::SystemTime::UNIX_EPOCH);
    let modified_ns = modified
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();

    let mut key = spec.source_path.display().to_string();
    key.push('|');
    key.push_str(&metadata.len().to_string());
    key.push('|');
    key.push_str(&modified_ns.to_string());

    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    let digest = hasher.finalize();
    let sha_hex: String = digest.iter().map(|b| format!("{:02x}", b)).collect();
    let sha8 = &sha_hex[..8];

    let codec_dir = match spec.preferred_codec {
        PreferredCodec::ProRes422 => "prores422",
    };

    let stem = spec
        .source_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("media");
    let filename = format!("{stem}__opt_{sha8}.mov");

    let output_path = root.join(codec_dir).join(&filename);
    let tmp_path = output_path.with_extension("mov.tmp");

    Ok((output_path, tmp_path, sha_hex))
}

fn spawn_event_dispatch(
    rx: Receiver<CacheEvent>,
    subscribers: Arc<Mutex<Vec<Sender<CacheEvent>>>>,
) {
    thread::Builder::new()
        .name("cache-events".to_string())
        .spawn(move || {
            while let Ok(event) = rx.recv() {
                let mut guard = subscribers.lock();
                guard.retain_mut(|tx| tx.send(event.clone()).is_ok());
            }
        })
        .expect("failed to spawn cache event dispatcher");
}

#[allow(clippy::too_many_arguments)]
fn run_worker(
    id: CacheJobId,
    spec: CacheJobSpec,
    output_path: PathBuf,
    tmp_path: PathBuf,
    cancel: Arc<AtomicBool>,
    inner: Arc<Mutex<State>>,
    events_tx: Sender<CacheEvent>,
    permits: Arc<JobSemaphore>,
) -> Result<()> {
    let _permit = permits.acquire();

    if cancel.load(Ordering::Relaxed) {
        finalize_job(id, JobFinish::Canceled, &inner, &events_tx, &tmp_path);
        return Ok(());
    }

    if let Err(err) = ensure_gst_init() {
        finalize_job(
            id,
            JobFinish::Failed(format!("gst init failed: {err}")),
            &inner,
            &events_tx,
            &tmp_path,
        );
        return Ok(());
    }

    {
        let mut guard = inner.lock();
        if let Some(job) = guard.jobs.get_mut(&id) {
            job.status = JobStatus::InProgress(0.0);
            job.started_at = Some(Instant::now());
        }
    }
    let _ = events_tx.send(CacheEvent::StatusChanged {
        id,
        status: JobStatus::InProgress(0.0),
    });

    if tmp_path.exists() {
        fs::remove_file(&tmp_path).ok();
    }

    let pipeline = match pipeline::build_prores_pipeline(&spec, &tmp_path)
        .with_context(|| format!("build pipeline for {}", spec.source_path.display()))
    {
        Ok(p) => p,
        Err(err) => {
            finalize_job(
                id,
                JobFinish::Failed(err.to_string()),
                &inner,
                &events_tx,
                &tmp_path,
            );
            return Ok(());
        }
    };

    let main_context = glib::MainContext::new();
    let cancel_for_loop = Arc::clone(&cancel);
    let inner_for_loop = Arc::clone(&inner);
    let events_for_loop = events_tx.clone();
    let output_for_loop = output_path.clone();

    let finish = match main_context.with_thread_default(move || {
        execute_pipeline(
            pipeline,
            cancel_for_loop,
            inner_for_loop,
            events_for_loop,
            output_for_loop,
            id,
        )
    }) {
        Ok(result) => result,
        Err(err) => {
            finalize_job(
                id,
                JobFinish::Failed(format!("main context error: {err}")),
                &inner,
                &events_tx,
                &tmp_path,
            );
            return Ok(());
        }
    };

    finalize_job(id, finish, &inner, &events_tx, &tmp_path);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn execute_pipeline(
    pipeline: gst::Pipeline,
    cancel: Arc<AtomicBool>,
    inner: Arc<Mutex<State>>,
    events_tx: Sender<CacheEvent>,
    output_path: PathBuf,
    id: CacheJobId,
) -> JobFinish {
    let main_loop = glib::MainLoop::new(None::<&glib::MainContext>, false);
    let (finish_tx, finish_rx) = mpsc::channel::<JobFinish>();
    let finished_flag = std::rc::Rc::new(std::cell::Cell::new(false));

    let bus = match pipeline.bus() {
        Some(bus) => bus,
        None => return JobFinish::Failed("pipeline missing message bus".into()),
    };

    let done_bus = finished_flag.clone();
    let finish_bus = finish_tx.clone();
    let loop_bus = main_loop.clone();
    let pipeline_bus = pipeline.clone();
    let output_bus = output_path.clone();
    bus.add_watch_local(move |_bus, message| {
        use gst::MessageView;
        match message.view() {
            MessageView::Eos(..) => {
                if done_bus.replace(true) {
                    return glib::ControlFlow::Break;
                }
                pipeline_bus.set_state(gst::State::Null).ok();
                let _ = finish_bus.send(JobFinish::Completed(output_bus.clone()));
                loop_bus.quit();
                glib::ControlFlow::Break
            }
            MessageView::Error(err) => {
                if done_bus.replace(true) {
                    return glib::ControlFlow::Break;
                }
                let message = format!(
                    "gstreamer error from {}: {} ({:?})",
                    err.src()
                        .map(|s| s.path_string())
                        .unwrap_or_else(|| "unknown".into()),
                    err.error(),
                    err.debug()
                );
                pipeline_bus.set_state(gst::State::Null).ok();
                let _ = finish_bus.send(JobFinish::Failed(message));
                loop_bus.quit();
                glib::ControlFlow::Break
            }
            MessageView::StateChanged(state) => {
                if state
                    .src()
                    .map(|s| s.is::<gst::Pipeline>())
                    .unwrap_or(false)
                {
                    debug!(
                        target = "cache",
                        id = id.0,
                        old = ?state.old(),
                        new = ?state.current(),
                        pending = ?state.pending(),
                        "pipeline state changed"
                    );
                }
                glib::ControlFlow::Continue
            }
            _ => glib::ControlFlow::Continue,
        }
    })
    .expect("add bus watch");

    let progress_flag = finished_flag.clone();
    let finish_cancel = finish_tx.clone();
    let loop_cancel = main_loop.clone();
    let pipeline_progress = pipeline.clone();
    let cancel_flag = Arc::clone(&cancel);
    let inner_progress = Arc::clone(&inner);
    let events_progress = events_tx.clone();
    let progress_cell = std::rc::Rc::new(std::cell::Cell::new(0.0f32));
    let progress_cell_clone = progress_cell.clone();

    glib::timeout_add_local(Duration::from_millis(100), move || {
        if cancel_flag.load(Ordering::Relaxed) {
            if progress_flag.replace(true) {
                return glib::ControlFlow::Break;
            }
            pipeline_progress.set_state(gst::State::Null).ok();
            let _ = finish_cancel.send(JobFinish::Canceled);
            loop_cancel.quit();
            return glib::ControlFlow::Break;
        }

        let position = pipeline_progress
            .query_position::<gst::ClockTime>()
            .unwrap_or(gst::ClockTime::ZERO);
        let duration = pipeline_progress
            .query_duration::<gst::ClockTime>()
            .unwrap_or_else(|| gst::ClockTime::from_seconds(1));
        if duration != gst::ClockTime::ZERO {
            let pct =
                (position.nseconds() as f64 / duration.nseconds() as f64).clamp(0.0, 1.0) as f32;
            let prev = progress_cell_clone.get();
            if (pct - prev).abs() >= 0.01 {
                progress_cell_clone.set(pct);
                {
                    let mut guard = inner_progress.lock();
                    if let Some(job) = guard.jobs.get_mut(&id) {
                        job.status = JobStatus::InProgress(pct);
                    }
                }
                let _ = events_progress.send(CacheEvent::StatusChanged {
                    id,
                    status: JobStatus::InProgress(pct),
                });
                debug!(
                    target = "cache",
                    id = id.0,
                    progress = pct,
                    "cache job progress"
                );
            }
        }
        glib::ControlFlow::Continue
    });

    if let Err(err) = pipeline.set_state(gst::State::Playing) {
        let detail = format!("set pipeline to Playing failed: {err}");
        let _ = finish_tx.send(JobFinish::Failed(detail.clone()));
        return finish_rx.recv().unwrap_or(JobFinish::Failed(detail));
    }

    main_loop.run();
    pipeline.set_state(gst::State::Null).ok();

    finish_rx
        .recv()
        .unwrap_or(JobFinish::Failed("pipeline exited without result".into()))
}

fn finalize_job(
    id: CacheJobId,
    finish: JobFinish,
    inner: &Arc<Mutex<State>>,
    events_tx: &Sender<CacheEvent>,
    tmp_path: &Path,
) {
    match &finish {
        JobFinish::Completed(final_path) => {
            if let Err(err) = fs::rename(tmp_path, final_path) {
                error!(
                    target = "cache",
                    id = id.0,
                    error = %err,
                    "failed to rename tmp to final; marking job failed"
                );
                finalize_job(
                    id,
                    JobFinish::Failed(format!("rename tmp to final failed: {err}")),
                    inner,
                    events_tx,
                    tmp_path,
                );
                return;
            }
        }
        JobFinish::Failed(_) | JobFinish::Canceled => {
            if tmp_path.exists() {
                fs::remove_file(tmp_path).ok();
            }
        }
    }

    let mut guard = inner.lock();
    let dedup_key = guard.jobs.get(&id).map(|job| job.dedup_key.clone());
    if let Some(job) = guard.jobs.get_mut(&id) {
        match finish {
            JobFinish::Completed(ref final_path) => {
                job.status = JobStatus::Completed(final_path.clone());
                debug!(
                    target = "cache",
                    id = id.0,
                    output = %final_path.display(),
                    "cache job completed"
                );
            }
            JobFinish::Failed(ref msg) => {
                job.status = JobStatus::Failed(msg.clone());
                warn!(target = "cache", id = id.0, error = %msg, "cache job failed");
            }
            JobFinish::Canceled => {
                job.status = JobStatus::Canceled;
                info!(target = "cache", id = id.0, "cache job canceled");
            }
        }
    }
    if matches!(finish, JobFinish::Failed(_) | JobFinish::Canceled) {
        if let Some(key) = dedup_key {
            guard.dedup.remove(&key);
        }
    }
    let status = guard.jobs.get(&id).map(|j| j.status.clone());
    drop(guard);

    if let Some(status) = status {
        let _ = events_tx.send(CacheEvent::StatusChanged { id, status });
    }
}

fn ensure_gst_init() -> Result<()> {
    use std::sync::OnceLock;
    static GST_INIT: OnceLock<Result<(), String>> = OnceLock::new();
    match GST_INIT.get_or_init(|| {
        gst::init()
            .map(|_| ())
            .map_err(|err| format!("gst::init failed: {err}"))
    }) {
        Ok(()) => Ok(()),
        Err(msg) => Err(anyhow!(msg.clone())),
    }
}
