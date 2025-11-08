mod app_assets;
mod app_cloud;
mod app_modal;
mod app_project;
mod app_screenplay;
mod app_storyboard;
mod app_timeline;
mod app_ui;

use crate::cache::job::{CacheEvent, CacheJobId, CacheJobSpec, PreferredCodec};
use crate::cache::CacheManager;
use crate::media_info::{HardwareCaps, MediaInfo as MediaInfoData, MediaKind};
use crate::playback_selector::{PlaybackSelector, PlaybackSource, ProxyMode};
use crate::prompt_normalize::normalize_prompt_in_place;
use crate::proxy_policy::should_proxy;
use crate::proxy_queue::{ProxyReason, ProxyStatus};
use serde::{Deserialize, Serialize};
use std::fmt::Write as FmtWrite;
use std::hash::{Hash, Hasher};

#[cfg(not(target_arch = "wasm32"))]
use raw_window_handle::{
    DisplayHandle, HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
    WindowHandle,
};

pub(crate) use app_modal::{PhaseAgg, PhasePlan};

// Preview behavior settings (frame-based thresholds)
#[derive(Clone, Copy)]
struct PreviewSettings {
    // Accept frames within this many frames when strict-paused
    strict_tolerance_frames: f32,
    // Accept frames within this many frames when non-strict paused
    paused_tolerance_frames: f32,
    // Only clear the last frame on seek if the target moved beyond this many frames
    clear_threshold_frames: f32,
}

impl Default for PreviewSettings {
    fn default() -> Self {
        Self {
            strict_tolerance_frames: 2.5,
            paused_tolerance_frames: 2.0,
            clear_threshold_frames: 2.0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AppMode {
    ProjectPicker,
    Editor,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WorkspaceView {
    Timeline,
    Chat,
    Storyboard,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScreenplayTab {
    Conversation,
    Draft,
}

#[derive(Clone, Debug)]
struct ChatMessage {
    role: ChatRole,
    text: String,
}

#[derive(Clone, Debug)]
pub(crate) struct PreviewCapture {
    path: std::path::PathBuf,
    timestamp: Instant,
    width: u32,
    height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChatRole {
    System,
    User,
    Assistant,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct StoryboardVideoSettings {
    filename_prefix: String,
    format: String,
    codec: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StoryboardAssetKind {
    Video,
    Image,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
enum StoryboardInputValue {
    Text(String),
    File(String),
    Integer(i64),
    Float(f64),
    Boolean(bool),
    Array(Vec<serde_json::Value>),
    Object(serde_json::Map<String, serde_json::Value>),
    Null,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum StoryboardWorkflowInputKind {
    Text { multiline: bool },
    File,
    Integer,
    Float,
    Boolean,
    Array,
    Object,
    Null,
}

impl StoryboardInputValue {
    fn from_json_with_kind(value: &serde_json::Value, kind: &StoryboardWorkflowInputKind) -> Self {
        match kind {
            StoryboardWorkflowInputKind::Text { .. } => {
                StoryboardInputValue::Text(value.as_str().unwrap_or_default().to_string())
            }
            StoryboardWorkflowInputKind::File => {
                StoryboardInputValue::File(value.as_str().unwrap_or_default().to_string())
            }
            StoryboardWorkflowInputKind::Integer => {
                StoryboardInputValue::Integer(value.as_i64().unwrap_or_default())
            }
            StoryboardWorkflowInputKind::Float => StoryboardInputValue::Float(
                value
                    .as_f64()
                    .or_else(|| value.as_i64().map(|v| v as f64))
                    .unwrap_or_default(),
            ),
            StoryboardWorkflowInputKind::Boolean => {
                StoryboardInputValue::Boolean(value.as_bool().unwrap_or(false))
            }
            StoryboardWorkflowInputKind::Array => {
                StoryboardInputValue::Array(value.as_array().cloned().unwrap_or_default())
            }
            StoryboardWorkflowInputKind::Object => {
                StoryboardInputValue::Object(value.as_object().cloned().unwrap_or_default())
            }
            StoryboardWorkflowInputKind::Null => StoryboardInputValue::Null,
        }
    }

    fn from_raw_json(value: serde_json::Value) -> Self {
        match value {
            serde_json::Value::String(s) => StoryboardInputValue::Text(s),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    StoryboardInputValue::Integer(i)
                } else {
                    StoryboardInputValue::Float(n.as_f64().unwrap_or_default())
                }
            }
            serde_json::Value::Bool(b) => StoryboardInputValue::Boolean(b),
            serde_json::Value::Array(arr) => StoryboardInputValue::Array(arr),
            serde_json::Value::Object(obj) => StoryboardInputValue::Object(obj),
            serde_json::Value::Null => StoryboardInputValue::Null,
        }
    }

    fn default_for_kind(kind: &StoryboardWorkflowInputKind) -> Self {
        match kind {
            StoryboardWorkflowInputKind::Text { .. } => StoryboardInputValue::Text(String::new()),
            StoryboardWorkflowInputKind::File => StoryboardInputValue::File(String::new()),
            StoryboardWorkflowInputKind::Integer => StoryboardInputValue::Integer(0),
            StoryboardWorkflowInputKind::Float => StoryboardInputValue::Float(0.0),
            StoryboardWorkflowInputKind::Boolean => StoryboardInputValue::Boolean(false),
            StoryboardWorkflowInputKind::Array => StoryboardInputValue::Array(Vec::new()),
            StoryboardWorkflowInputKind::Object => {
                StoryboardInputValue::Object(serde_json::Map::new())
            }
            StoryboardWorkflowInputKind::Null => StoryboardInputValue::Null,
        }
    }

    fn matches_kind(&self, kind: &StoryboardWorkflowInputKind) -> bool {
        match (self, kind) {
            (StoryboardInputValue::Text(_), StoryboardWorkflowInputKind::Text { .. }) => true,
            (StoryboardInputValue::File(_), StoryboardWorkflowInputKind::File) => true,
            (StoryboardInputValue::Integer(_), StoryboardWorkflowInputKind::Integer) => true,
            (StoryboardInputValue::Float(_), StoryboardWorkflowInputKind::Float) => true,
            (StoryboardInputValue::Boolean(_), StoryboardWorkflowInputKind::Boolean) => true,
            (StoryboardInputValue::Array(_), StoryboardWorkflowInputKind::Array) => true,
            (StoryboardInputValue::Object(_), StoryboardWorkflowInputKind::Object) => true,
            (StoryboardInputValue::Null, StoryboardWorkflowInputKind::Null) => true,
            _ => false,
        }
    }

    fn to_json(&self) -> serde_json::Value {
        match self {
            StoryboardInputValue::Text(s) | StoryboardInputValue::File(s) => {
                serde_json::Value::String(s.clone())
            }
            StoryboardInputValue::Integer(i) => serde_json::Value::Number((*i).into()),
            StoryboardInputValue::Float(f) => serde_json::Number::from_f64(*f)
                .map_or(serde_json::Value::Null, serde_json::Value::Number),
            StoryboardInputValue::Boolean(b) => serde_json::Value::Bool(*b),
            StoryboardInputValue::Array(arr) => serde_json::Value::Array(arr.clone()),
            StoryboardInputValue::Object(obj) => serde_json::Value::Object(obj.clone()),
            StoryboardInputValue::Null => serde_json::Value::Null,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoryboardWorkflowInputSpec {
    map_key: String,
    node_id: String,
    input_key: String,
    group_label: String,
    label: String,
    kind: StoryboardWorkflowInputKind,
    default_value: Option<StoryboardInputValue>,
    #[serde(default)]
    node_class: String,
    #[serde(skip)]
    semantic: WorkflowInputSemantic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkflowInputSemantic {
    PromptPositive,
    PromptNegative,
    PromptSingle,
    Ratio,
    AspectRatio,
    Other,
}

impl Default for WorkflowInputSemantic {
    fn default() -> Self {
        WorkflowInputSemantic::Other
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum StoryboardWorkflowOutputKind {
    Image,
    Video,
    #[serde(alias = "image-video", alias = "image_video", alias = "imageandvideo")]
    ImageAndVideo,
}

impl Default for StoryboardWorkflowOutputKind {
    fn default() -> Self {
        StoryboardWorkflowOutputKind::Image
    }
}

impl StoryboardWorkflowOutputKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            StoryboardWorkflowOutputKind::Image => "Outputs: Image",
            StoryboardWorkflowOutputKind::Video => "Outputs: Video",
            StoryboardWorkflowOutputKind::ImageAndVideo => "Outputs: Image & Video",
        }
    }

    fn from_flags(image: bool, video: bool) -> Self {
        match (image, video) {
            (true, true) => StoryboardWorkflowOutputKind::ImageAndVideo,
            (true, false) => StoryboardWorkflowOutputKind::Image,
            (false, true) => StoryboardWorkflowOutputKind::Video,
            (false, false) => StoryboardWorkflowOutputKind::Image,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoryboardCard {
    id: uuid::Uuid,
    title: String,
    description: String,
    reference_path: String,
    duration_seconds: f32,
    preview_error: Option<String>,
    workflow_id: Option<uuid::Uuid>,
    workflow_error: Option<String>,
    workflow_status: Option<String>,
    video_settings: Option<StoryboardVideoSettings>,
    #[serde(default)]
    output_kind: StoryboardWorkflowOutputKind,
    workflow_inputs: std::collections::HashMap<String, StoryboardInputValue>,
    workflow_input_errors: std::collections::HashMap<String, String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct PersistedStoryboard {
    cards: Vec<StoryboardCard>,
    selected: Option<usize>,
    #[serde(default)]
    comfy_jobs: std::collections::HashMap<uuid::Uuid, PersistedComfyJob>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedComfyJob {
    prompt_id: Option<String>,
    prefix: String,
    workflow_name: Option<String>,
    card_title: String,
    card_description: String,
    reference_path: String,
    duration_seconds: f32,
    fps: f32,
    video_settings: Option<StoryboardVideoSettings>,
    workflow_inputs: serde_json::Value,
    last_output: Option<std::path::PathBuf>,
    queued_at: chrono::DateTime<chrono::Utc>,
}

impl From<PersistedComfyJob> for ComfyStoryboardJob {
    fn from(value: PersistedComfyJob) -> Self {
        ComfyStoryboardJob {
            prompt_id: value.prompt_id,
            prefix: value.prefix,
            workflow_name: value.workflow_name,
            card_title: value.card_title,
            card_description: value.card_description,
            reference_path: value.reference_path,
            duration_seconds: value.duration_seconds,
            fps: value.fps,
            video_settings: value.video_settings,
            workflow_inputs: value.workflow_inputs,
            last_output: value.last_output,
            queued_at: value.queued_at,
        }
    }
}

impl From<&ComfyStoryboardJob> for PersistedComfyJob {
    fn from(job: &ComfyStoryboardJob) -> Self {
        PersistedComfyJob {
            prompt_id: job.prompt_id.clone(),
            prefix: job.prefix.clone(),
            workflow_name: job.workflow_name.clone(),
            card_title: job.card_title.clone(),
            card_description: job.card_description.clone(),
            reference_path: job.reference_path.clone(),
            duration_seconds: job.duration_seconds,
            fps: job.fps,
            video_settings: job.video_settings.clone(),
            workflow_inputs: job.workflow_inputs.clone(),
            last_output: job.last_output.clone(),
            queued_at: job.queued_at,
        }
    }
}

#[derive(Clone, Debug)]
struct StoryboardWorkflowPreset {
    id: uuid::Uuid,
    name: String,
    path: std::path::PathBuf,
    builtin: bool,
    video_defaults: Option<StoryboardVideoSettings>,
    output_kind: StoryboardWorkflowOutputKind,
    input_specs: Vec<StoryboardWorkflowInputSpec>,
}

const DEFAULT_STORYBOARD_WORKFLOW_NAME: &str = "default (image)";
const DEFAULT_STORYBOARD_WORKFLOW_FILE: &str = "default_image_storyboard.json";
const DEFAULT_STORYBOARD_WORKFLOW_JSON: &str =
    include_str!("../../../default_image_storyboard.json");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ComfyJobStatus {
    Queued,
    Running,
    Completed,
    Failed,
}

#[derive(Clone, Debug)]
struct ComfyJobInfo {
    status: ComfyJobStatus,
    progress: f32,
    updated_at: Instant,
}

#[derive(Debug)]
enum ComfyWsEvent {
    Queue {
        pending: Vec<String>,
        running: Vec<String>,
    },
    Progress {
        prompt_id: String,
        value: f32,
        max: f32,
    },
    ExecutionStart {
        prompt_id: String,
    },
    ExecutionEnd {
        prompt_id: String,
    },
}

#[derive(Clone, Debug)]
struct ComfyStoryboardJob {
    prompt_id: Option<String>,
    prefix: String,
    workflow_name: Option<String>,
    card_title: String,
    card_description: String,
    reference_path: String,
    duration_seconds: f32,
    fps: f32,
    video_settings: Option<StoryboardVideoSettings>,
    workflow_inputs: serde_json::Value,
    last_output: Option<std::path::PathBuf>,
    queued_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ComfyAlertKind {
    Info,
    Success,
    Warning,
}

struct ComfyAlert {
    message: String,
    kind: ComfyAlertKind,
    expires_at: Instant,
}

#[derive(Clone)]
enum StoryboardPendingInputRefresh {
    All,
    Keys(std::collections::HashSet<String>),
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum AutoProxySetting {
    Off,
    LargeOnly,
    All,
}

impl Default for AutoProxySetting {
    fn default() -> Self {
        AutoProxySetting::LargeOnly
    }
}

impl AutoProxySetting {
    fn display_name(self) -> &'static str {
        match self {
            AutoProxySetting::Off => "Off",
            AutoProxySetting::LargeOnly => "Large Only",
            AutoProxySetting::All => "All Clips",
        }
    }

    fn should_queue_proxy(
        self,
        is_video: bool,
        media_info: Option<&MediaInfoData>,
        hardware: &HardwareCaps,
    ) -> bool {
        match self {
            AutoProxySetting::Off => false,
            AutoProxySetting::All => is_video,
            AutoProxySetting::LargeOnly => {
                is_video
                    && media_info
                        .map(|info| should_proxy(info, hardware))
                        .unwrap_or(false)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ViewerScale {
    Full,
    Half,
    Quarter,
}

impl ViewerScale {
    fn factor(self) -> f32 {
        match self {
            ViewerScale::Full => 1.0,
            ViewerScale::Half => 0.5,
            ViewerScale::Quarter => 0.25,
        }
    }

    fn label(self) -> &'static str {
        match self {
            ViewerScale::Full => "100%",
            ViewerScale::Half => "50%",
            ViewerScale::Quarter => "25%",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedProxySettings {
    #[serde(default)]
    proxy_mode: ProxyMode,
    #[serde(default)]
    auto_proxy: AutoProxySetting,
}

impl Default for PersistedProxySettings {
    fn default() -> Self {
        Self {
            proxy_mode: ProxyMode::OriginalOptimized,
            auto_proxy: AutoProxySetting::LargeOnly,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PersistedComfySettings {
    #[serde(default)]
    api_key: String,
}

struct CachedAssetEntry {
    asset: project::AssetRow,
    last_refresh: Instant,
}

struct PlaybackPathDecision {
    decode_path: String,
    asset: Option<project::AssetRow>,
    using_proxy: bool,
    using_optimized: bool,
    queue_reason: Option<ProxyReason>,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy)]
struct FileDialogParent {
    window: RawWindowHandle,
    display: RawDisplayHandle,
}

#[cfg(not(target_arch = "wasm32"))]
impl HasWindowHandle for FileDialogParent {
    fn window_handle(&self) -> Result<WindowHandle<'_>, raw_window_handle::HandleError> {
        unsafe { Ok(WindowHandle::borrow_raw(self.window)) }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl HasDisplayHandle for FileDialogParent {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, raw_window_handle::HandleError> {
        unsafe { Ok(DisplayHandle::borrow_raw(self.display)) }
    }
}

fn uuid_from_path(path: &std::path::Path) -> uuid::Uuid {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let key = canonical.to_string_lossy();
    let mut hasher_hi = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher_hi);
    let hi = hasher_hi.finish();
    let mut hasher_lo = std::collections::hash_map::DefaultHasher::new();
    format!("{}#", key).hash(&mut hasher_lo);
    let lo = hasher_lo.finish();
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&hi.to_be_bytes());
    bytes[8..].copy_from_slice(&lo.to_be_bytes());
    uuid::Uuid::from_bytes(bytes)
}

impl StoryboardWorkflowPreset {
    fn from_path(path: std::path::PathBuf, builtin: bool) -> Option<Self> {
        let canonical = path.canonicalize().unwrap_or(path.clone());
        let stem = canonical.file_stem()?.to_string_lossy().to_string();
        let id = uuid_from_path(&canonical);
        let video_defaults = Self::load_video_defaults(&canonical);
        let output_kind = Self::load_output_kind(&canonical);
        let input_specs = Self::load_input_specs(&canonical);
        let name = if builtin {
            DEFAULT_STORYBOARD_WORKFLOW_NAME.to_string()
        } else {
            stem
        };
        Some(Self {
            id,
            name,
            path: canonical,
            builtin,
            video_defaults,
            output_kind,
            input_specs,
        })
    }

    fn load_video_defaults(path: &std::path::Path) -> Option<StoryboardVideoSettings> {
        let data = std::fs::read_to_string(path).ok()?;
        Self::parse_video_defaults(&data)
    }

    fn load_output_kind(path: &std::path::Path) -> StoryboardWorkflowOutputKind {
        let data = match std::fs::read_to_string(path) {
            Ok(data) => data,
            Err(_) => return StoryboardWorkflowOutputKind::Image,
        };
        Self::parse_output_kind(&data)
    }

    fn parse_output_kind(data: &str) -> StoryboardWorkflowOutputKind {
        let mut image = false;
        let mut video = false;
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(data) {
            Self::scan_value_for_outputs(&value, &mut image, &mut video);
        }
        if !image || !video {
            if let Some(prompt) = Self::prompt_map_from_str(data) {
                Self::scan_prompt_for_outputs(&prompt, &mut image, &mut video);
            }
        }
        StoryboardWorkflowOutputKind::from_flags(image, video)
    }

    fn scan_prompt_for_outputs(
        prompt: &serde_json::Map<String, serde_json::Value>,
        image: &mut bool,
        video: &mut bool,
    ) {
        for (node_id, node_value) in prompt {
            Self::inspect_identifier(node_id, image, video);
            if let Some(obj) = node_value.as_object() {
                if let Some(class_type) = obj.get("class_type").and_then(|v| v.as_str()) {
                    Self::inspect_identifier(class_type, image, video);
                }
                if let Some(title) = Self::extract_node_title(obj) {
                    Self::inspect_identifier(&title, image, video);
                }
                if let Some(inputs) = obj.get("inputs") {
                    Self::scan_value_for_outputs(inputs, image, video);
                }
            }
        }
    }

    fn scan_value_for_outputs(value: &serde_json::Value, image: &mut bool, video: &mut bool) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, val) in map {
                    Self::inspect_identifier(key, image, video);
                    Self::scan_value_for_outputs(val, image, video);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    Self::scan_value_for_outputs(item, image, video);
                }
            }
            serde_json::Value::String(text) => {
                Self::inspect_identifier(text, image, video);
            }
            _ => {}
        }
    }

    fn inspect_identifier(text: &str, image: &mut bool, video: &mut bool) {
        let lower = text.to_ascii_lowercase();
        if lower.contains("save image") || lower.contains("saveimage") {
            *image = true;
        }
        if lower.contains("save video")
            || lower.contains("savevideo")
            || lower.contains("vhs_videocombine")
        {
            *video = true;
        }
    }

    fn parse_video_defaults(data: &str) -> Option<StoryboardVideoSettings> {
        let value: serde_json::Value = serde_json::from_str(data).ok()?;
        if let Some(prompt) = value.get("prompt").and_then(|p| p.as_object()) {
            for node in prompt.values() {
                if node
                    .get("class_type")
                    .and_then(|ct| ct.as_str())
                    .map(|ct| ct == "SaveVideo")
                    .unwrap_or(false)
                {
                    if let Some(inputs) = node.get("inputs").and_then(|i| i.as_object()) {
                        let filename = inputs
                            .get("filename_prefix")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                            .or_else(|| inputs.get("filename_prefix").map(|v| v.to_string()));
                        let format = inputs
                            .get("format")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                            .or_else(|| inputs.get("format").map(|v| v.to_string()));
                        let codec = inputs
                            .get("codec")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                            .or_else(|| inputs.get("codec").map(|v| v.to_string()));
                        if let (Some(filename_prefix), Some(format), Some(codec)) =
                            (filename, format, codec)
                        {
                            return Some(StoryboardVideoSettings {
                                filename_prefix,
                                format,
                                codec,
                            });
                        }
                    }
                }
            }
        }
        if let Some(nodes) = value.get("nodes").and_then(|n| n.as_array()) {
            for node in nodes {
                if node
                    .get("type")
                    .and_then(|t| t.as_str())
                    .map(|t| t == "SaveVideo")
                    .unwrap_or(false)
                {
                    if let Some(widgets) = node.get("widgets_values").and_then(|w| w.as_array()) {
                        let filename_prefix = widgets
                            .get(0)
                            .and_then(|v| v.as_str())
                            .unwrap_or("video/ComfyUI")
                            .to_string();
                        let format = widgets
                            .get(1)
                            .and_then(|v| v.as_str())
                            .unwrap_or("auto")
                            .to_string();
                        let codec = widgets
                            .get(2)
                            .and_then(|v| v.as_str())
                            .unwrap_or("auto")
                            .to_string();
                        return Some(StoryboardVideoSettings {
                            filename_prefix,
                            format,
                            codec,
                        });
                    }
                }
            }
        }
        None
    }

    fn load_input_specs(path: &std::path::Path) -> Vec<StoryboardWorkflowInputSpec> {
        let data = match std::fs::read_to_string(path) {
            Ok(data) => data,
            Err(_) => return Vec::new(),
        };
        let prompt = match Self::prompt_map_from_str(&data) {
            Some(prompt) => prompt,
            None => return Vec::new(),
        };
        Self::collect_input_specs(prompt)
    }

    fn prompt_map_from_str(data: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(data) {
            if let Some(obj) = value.get("prompt").and_then(|p| p.as_object()).cloned() {
                return Some(obj);
            }
        }
        let converted = crate::app_cloud::convert_workflow_to_prompt(data).ok()?;
        let converted_value = serde_json::from_str::<serde_json::Value>(&converted).ok()?;
        converted_value
            .get("prompt")
            .and_then(|p| p.as_object())
            .cloned()
    }

    fn collect_input_specs(
        prompt: serde_json::Map<String, serde_json::Value>,
    ) -> Vec<StoryboardWorkflowInputSpec> {
        let mut specs = Vec::new();
        for (node_id, node_value) in prompt {
            let node_obj = match node_value.as_object() {
                Some(obj) => obj,
                None => continue,
            };
            let class_type = node_obj
                .get("class_type")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if Self::is_output_node(class_type) {
                continue;
            }
            let title_string = Self::extract_node_title(node_obj);
            let title = title_string.as_deref();
            let is_gemini_image = Self::is_gemini_image_node(class_type, title);
            let inputs_map = match node_obj.get("inputs").and_then(|inputs| inputs.as_object()) {
                Some(inputs) => inputs,
                None => continue,
            };
            for (input_key, input_value) in inputs_map {
                if input_key.starts_with("$$") {
                    continue;
                }
                let is_prompt_input = is_gemini_image && input_key.eq_ignore_ascii_case("prompt");
                let is_aspect_ratio_input = is_gemini_image
                    && (input_key.eq_ignore_ascii_case("aspect_ratio")
                        || input_key.eq_ignore_ascii_case("aspect-ratio"));
                if is_gemini_image && !is_prompt_input && !is_aspect_ratio_input {
                    continue;
                }
                let mut kind = Self::infer_input_kind(class_type, title, input_key, input_value);
                if is_prompt_input {
                    kind = StoryboardWorkflowInputKind::Text { multiline: true };
                } else if is_aspect_ratio_input {
                    kind = StoryboardWorkflowInputKind::Text { multiline: false };
                }
                if matches!(kind, StoryboardWorkflowInputKind::Array) {
                    continue;
                }
                let default_value = Some(StoryboardInputValue::from_json_with_kind(
                    input_value,
                    &kind,
                ));
                let map_key = format!("{}:{}", node_id, input_key);
                let group_label = Self::format_group_label(title, class_type, node_id.as_str());
                let label = Self::format_input_label(title, class_type, input_key);
                let semantic = Self::infer_input_semantic(input_key, &label, class_type);
                specs.push(StoryboardWorkflowInputSpec {
                    map_key,
                    node_id: node_id.clone(),
                    input_key: input_key.clone(),
                    group_label,
                    label,
                    kind,
                    default_value,
                    node_class: class_type.to_string(),
                    semantic,
                });
            }
        }
        specs
    }

    fn extract_node_title(node_obj: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
        if let Some(title) = node_obj
            .get("_meta")
            .and_then(|m| m.as_object())
            .and_then(|meta| meta.get("title"))
            .and_then(|t| t.as_str())
        {
            let trimmed = title.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        if let Some(title) = node_obj.get("title").and_then(|v| v.as_str()) {
            let trimmed = title.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        None
    }

    fn infer_input_kind(
        class_type: &str,
        title: Option<&str>,
        input_key: &str,
        value: &serde_json::Value,
    ) -> StoryboardWorkflowInputKind {
        let class_lower = class_type.to_ascii_lowercase();
        let key_lower = input_key.to_ascii_lowercase();
        let title_lower = title.map(|t| t.to_ascii_lowercase());

        let value_is_string = value.as_str().is_some();
        let value_is_null = value.is_null();

        let file_keywords = ["image", "video", "file", "path"];
        let is_file_keyword = file_keywords.iter().any(|kw| key_lower.contains(kw));
        let title_suggests_file = title_lower
            .as_ref()
            .map(|t| t.contains("image") || t.contains("reference") || t.contains("video"))
            .unwrap_or(false);
        let is_file_node = class_lower.contains("loadimage")
            || class_lower.contains("loadvideo")
            || class_lower.contains("imageinput")
            || class_lower.contains("videoinput")
            || class_lower.contains("upload");

        if (value_is_string || value_is_null)
            && (is_file_keyword || is_file_node || title_suggests_file)
        {
            return StoryboardWorkflowInputKind::File;
        }

        match value {
            serde_json::Value::String(s) => {
                let mut multiline = false;
                if key_lower.contains("prompt")
                    || key_lower.contains("description")
                    || key_lower.contains("text")
                {
                    multiline = true;
                }
                if let Some(title_lower) = title_lower.as_ref() {
                    if title_lower.contains("prompt")
                        || title_lower.contains("description")
                        || title_lower.contains("notes")
                    {
                        multiline = true;
                    }
                }
                if class_lower.contains("textencode") || class_lower.contains("prompt") {
                    multiline = true;
                }
                if s.len() > 120 || s.contains('\n') {
                    multiline = true;
                }
                StoryboardWorkflowInputKind::Text { multiline }
            }
            serde_json::Value::Bool(_) => StoryboardWorkflowInputKind::Boolean,
            serde_json::Value::Number(num) => {
                if num.is_i64() {
                    StoryboardWorkflowInputKind::Integer
                } else {
                    StoryboardWorkflowInputKind::Float
                }
            }
            serde_json::Value::Array(_) => StoryboardWorkflowInputKind::Array,
            serde_json::Value::Object(_) => StoryboardWorkflowInputKind::Object,
            serde_json::Value::Null => StoryboardWorkflowInputKind::Null,
        }
    }

    fn infer_input_semantic(input_key: &str, label: &str, class_type: &str) -> WorkflowInputSemantic {
        let key = input_key.to_ascii_lowercase();
        let label_lower = label.to_ascii_lowercase();
        if key.contains("negative") || label_lower.contains("negative") {
            return WorkflowInputSemantic::PromptNegative;
        }
        if key.contains("prompt") || label_lower.contains("prompt") {
            if key.contains("positive") || label_lower.contains("positive") {
                return WorkflowInputSemantic::PromptPositive;
            }
            if key.contains("negative") || label_lower.contains("negative") {
                return WorkflowInputSemantic::PromptNegative;
            }
            return WorkflowInputSemantic::PromptSingle;
        }
        if key.contains("ratio") || label_lower.contains("ratio") {
            if key.contains("aspect") || label_lower.contains("aspect") {
                return WorkflowInputSemantic::AspectRatio;
            }
            return WorkflowInputSemantic::Ratio;
        }
        if class_type.eq_ignore_ascii_case("RunwayImageToVideoNodeGen4") {
            if key == "ratio" {
                return WorkflowInputSemantic::Ratio;
            }
        }
        WorkflowInputSemantic::Other
    }

    fn is_placeholder_title(title: &str) -> bool {
        let trimmed = title.trim();
        let lower = trimmed.to_ascii_lowercase();
        lower.contains("node name for s&r") || lower.contains("node name for s & r")
    }

    fn format_input_label(title: Option<&str>, class_type: &str, input_key: &str) -> String {
        match title {
            Some(title) if !title.is_empty() && !Self::is_placeholder_title(title) => {
                if input_key.eq_ignore_ascii_case("text") {
                    title.to_string()
                } else {
                    format!("{} ({})", title, input_key)
                }
            }
            _ => {
                if class_type.is_empty() {
                    input_key.to_string()
                } else {
                    format!("{} ({})", class_type, input_key)
                }
            }
        }
    }

    fn format_group_label(title: Option<&str>, class_type: &str, node_id: &str) -> String {
        if let Some(title) = title {
            if !title.is_empty() && !Self::is_placeholder_title(title) {
                return title.to_string();
            }
        }
        if !class_type.is_empty() {
            return class_type.to_string();
        }
        format!("Node {}", node_id)
    }

    fn is_gemini_image_node(class_type: &str, title: Option<&str>) -> bool {
        if class_type.eq_ignore_ascii_case("GeminiImageNode") {
            return true;
        }
        if let Some(title) = title {
            let trimmed = title.trim();
            if !trimmed.is_empty() && trimmed.to_ascii_lowercase().eq("google gemini image") {
                return true;
            }
        }
        false
    }

    fn is_output_node(class_type: &str) -> bool {
        let lower = class_type.trim().to_ascii_lowercase();
        matches!(
            lower.as_str(),
            "savevideo"
                | "savevideoadvanced"
                | "saveimage"
                | "saveimages"
                | "savegif"
                | "saveaudio"
                | "previewimage"
                | "previewvideo"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn collect_input_specs_filters_gemini_extras() {
        let workflow = json!({
            "prompt": {
                "5": {
                    "inputs": {
                        "prompt": "A test prompt",
                        "model": "gemini-2.5-flash-image-preview",
                        "aspect_ratio": "16:9",
                        "seed": 970489684225986_u64,
                        "$$node-chat-history": ""
                    },
                    "class_type": "GeminiImageNode",
                    "_meta": {
                        "title": "Google Gemini Image"
                    }
                }
            }
        });

        let prompt_map = workflow
            .get("prompt")
            .and_then(|v| v.as_object())
            .cloned()
            .expect("prompt entries");
        let specs = StoryboardWorkflowPreset::collect_input_specs(prompt_map);
        assert_eq!(
            specs.len(),
            2,
            "expected only prompt and aspect ratio specs"
        );
        let keys: Vec<_> = specs.iter().map(|spec| spec.input_key.as_str()).collect();
        assert!(keys.contains(&"prompt"));
        assert!(keys.contains(&"aspect_ratio"));
        assert!(specs.iter().all(|spec| !spec.input_key.starts_with("$$")));
        assert!(specs
            .iter()
            .all(|spec| spec.node_class.eq_ignore_ascii_case("GeminiImageNode")));
    }

    #[test]
    fn infer_input_semantic_detects_prompt_variants() {
        assert_eq!(
            StoryboardWorkflowPreset::infer_input_semantic(
                "positive_prompt",
                "Positive Prompt",
                "TestNode"
            ),
            WorkflowInputSemantic::PromptPositive
        );
        assert_eq!(
            StoryboardWorkflowPreset::infer_input_semantic(
                "negativePrompt",
                "Negative Prompt",
                "TestNode"
            ),
            WorkflowInputSemantic::PromptNegative
        );
        assert_eq!(
            StoryboardWorkflowPreset::infer_input_semantic(
                "prompt",
                "Story Prompt",
                "TestNode"
            ),
            WorkflowInputSemantic::PromptSingle
        );
        assert_eq!(
            StoryboardWorkflowPreset::infer_input_semantic(
                "aspect_ratio",
                "Aspect Ratio",
                "GeminiImageNode"
            ),
            WorkflowInputSemantic::AspectRatio
        );
        assert_eq!(
            StoryboardWorkflowPreset::infer_input_semantic(
                "ratio",
                "Ratio",
                "RunwayImageToVideoNodeGen4"
            ),
            WorkflowInputSemantic::Ratio
        );
    }

    fn make_text_spec(
        map_key: &str,
        label: &str,
        semantic: WorkflowInputSemantic,
    ) -> StoryboardWorkflowInputSpec {
        StoryboardWorkflowInputSpec {
            map_key: map_key.to_string(),
            node_id: map_key.to_string(),
            input_key: map_key.to_string(),
            group_label: "Group".to_string(),
            label: label.to_string(),
            kind: StoryboardWorkflowInputKind::Text { multiline: false },
            default_value: Some(StoryboardInputValue::Text(String::new())),
            node_class: "TestNode".to_string(),
            semantic,
        }
    }

    fn make_preset(specs: Vec<StoryboardWorkflowInputSpec>) -> StoryboardWorkflowPreset {
        StoryboardWorkflowPreset {
            id: uuid::Uuid::new_v4(),
            name: "preset".to_string(),
            path: std::path::PathBuf::new(),
            builtin: false,
            video_defaults: None,
            output_kind: StoryboardWorkflowOutputKind::Image,
            input_specs: specs,
        }
    }

    fn blank_card() -> StoryboardCard {
        StoryboardCard {
            id: uuid::Uuid::new_v4(),
            title: "Test".to_string(),
            description: String::new(),
            reference_path: String::new(),
            duration_seconds: 5.0,
            preview_error: None,
            workflow_id: None,
            workflow_error: None,
            workflow_status: None,
            video_settings: None,
            output_kind: StoryboardWorkflowOutputKind::Image,
            workflow_inputs: HashMap::new(),
            workflow_input_errors: HashMap::new(),
        }
    }

    #[test]
    fn sync_storyboard_inputs_transfers_prompt_and_ratio_fields() {
        let old_preset = make_preset(vec![
            make_text_spec(
                "old:positive",
                "Positive Prompt",
                WorkflowInputSemantic::PromptPositive,
            ),
            make_text_spec(
                "old:negative",
                "Negative Prompt",
                WorkflowInputSemantic::PromptNegative,
            ),
            make_text_spec("old:ratio", "Ratio", WorkflowInputSemantic::Ratio),
        ]);
        let new_preset = make_preset(vec![
            make_text_spec("new:prompt", "Prompt", WorkflowInputSemantic::PromptSingle),
            make_text_spec(
                "new:aspect",
                "Aspect Ratio",
                WorkflowInputSemantic::AspectRatio,
            ),
        ]);
        let mut card = blank_card();
        card.workflow_inputs.insert(
            "old:positive".to_string(),
            StoryboardInputValue::Text("Bright scene".to_string()),
        );
        card.workflow_inputs.insert(
            "old:negative".to_string(),
            StoryboardInputValue::Text("Noise".to_string()),
        );
        card.workflow_inputs.insert(
            "old:ratio".to_string(),
            StoryboardInputValue::Text("16:9".to_string()),
        );

        App::sync_storyboard_inputs_with_transfer(&mut card, &new_preset, Some(&old_preset));

        assert_eq!(
            card.workflow_inputs.get("new:prompt"),
            Some(&StoryboardInputValue::Text("Bright scene".to_string()))
        );
        assert_eq!(
            card.workflow_inputs.get("new:aspect"),
            Some(&StoryboardInputValue::Text("16:9".to_string()))
        );
    }

    #[test]
    fn sync_storyboard_inputs_transfers_single_prompt_to_dual() {
        let single_prompt = make_preset(vec![make_text_spec(
            "single:prompt",
            "Prompt",
            WorkflowInputSemantic::PromptSingle,
        )]);
        let dual_prompt = make_preset(vec![
            make_text_spec(
                "dual:positive",
                "Positive Prompt",
                WorkflowInputSemantic::PromptPositive,
            ),
            make_text_spec(
                "dual:negative",
                "Negative Prompt",
                WorkflowInputSemantic::PromptNegative,
            ),
        ]);
        let mut card = blank_card();
        card.workflow_inputs.insert(
            "single:prompt".to_string(),
            StoryboardInputValue::Text("Moody city street".to_string()),
        );

        App::sync_storyboard_inputs_with_transfer(&mut card, &dual_prompt, Some(&single_prompt));

        assert_eq!(
            card.workflow_inputs.get("dual:positive"),
            Some(&StoryboardInputValue::Text("Moody city street".to_string()))
        );
        // Negative prompt should fall back to default empty text
        assert_eq!(
            card.workflow_inputs.get("dual:negative"),
            Some(&StoryboardInputValue::Text(String::new()))
        );
    }

    #[test]
    fn sync_storyboard_inputs_preserves_values_when_refreshing_same_preset() {
        let preset = make_preset(vec![make_text_spec(
            "prompt:main",
            "Prompt",
            WorkflowInputSemantic::PromptSingle,
        )]);
        let mut card = blank_card();
        card.workflow_inputs.insert(
            "prompt:main".to_string(),
            StoryboardInputValue::Text("Keep me".to_string()),
        );

        App::sync_storyboard_inputs_with_transfer(&mut card, &preset, Some(&preset));

        assert_eq!(
            card.workflow_inputs.get("prompt:main"),
            Some(&StoryboardInputValue::Text("Keep me".to_string()))
        );
    }
}
enum ChatEvent {
    Response(Result<String>),
}

const CHAT_HISTORY_LIMIT: usize = 60;
const CHAT_PROMPT_WINDOW: usize = 24;

struct App {
    db: ProjectDb,
    project_id: String,
    import_path: String,
    // timeline state
    seq: Sequence,
    timeline_history: CommandHistory,
    zoom_px_per_frame: f32,
    playhead: i64,
    playing: bool,
    last_tick: Option<Instant>,
    // Anchored playhead timing to avoid jitter
    play_anchor_instant: Option<Instant>,
    play_anchor_frame: i64,
    preview: PreviewState,
    audio_out: Option<audio_engine::AudioEngine>,
    selected: Option<(usize, usize)>,
    drag: Option<DragState>,
    export: ExportUiState,
    import_workers: Vec<std::thread::JoinHandle<()>>,
    jobs: Option<jobs_crate::JobsHandle>,
    job_events: Vec<JobEvent>,
    show_jobs: bool,
    hardware_caps: Arc<HardwareCaps>,
    proxy_queue: Option<crate::proxy_queue::ProxyQueue>,
    proxy_events: Option<Receiver<crate::proxy_queue::ProxyEvent>>,
    proxy_status: std::collections::HashMap<String, ProxyStatus>,
    proxy_logs: std::collections::HashMap<String, std::collections::VecDeque<String>>,
    proxy_mode_user: ProxyMode,
    proxy_mode_override: Option<ProxyMode>,
    proxy_preview_overrides: std::collections::HashSet<String>,
    auto_proxy_setting: AutoProxySetting,
    cache_manager: CacheManager,
    cache_events: std::sync::mpsc::Receiver<CacheEvent>,
    cache_job_status: std::collections::HashMap<CacheJobId, crate::cache::job::JobStatus>,
    viewer_scale: ViewerScale,
    playback_lag_frames: u32,
    playback_stable_frames: u32,
    asset_cache: std::collections::HashMap<String, CachedAssetEntry>,
    auto_proxy_requests: std::collections::HashSet<String>,
    auto_analysis_requests: std::collections::HashSet<String>,
    pending_heavy_assets: std::collections::VecDeque<String>,
    pending_heavy_asset_set: std::collections::HashSet<String>,
    last_heavy_job_dispatch: Option<Instant>,
    decode_mgr: DecodeManager,
    playback_clock: PlaybackClock,
    audio_cache: AudioCache,
    audio_buffers: AudioBufferCache,
    // When true during this frame, enable audible scrubbing while paused
    // Last successfully presented key: (source path, media time in milliseconds)
    // Using media time (not playhead frame) avoids wrong reuse when clips share a path but have different in_offset/rate.
    last_preview_key: Option<(String, i64)>,
    // Playback engine
    engine: EngineState,
    // Debounce decode commands: remember last sent (state, path, optional seek bucket)
    last_sent: Option<(PlayState, String, Option<i64>)>,
    // Epsilon-based dispatch tracking
    last_seek_sent_pts: Option<f64>,
    last_play_reanchor_time: Option<Instant>,
    // Throttled engine log state
    // (Used only for preview_ui logging when sending worker commands)
    // Not strictly necessary, but kept for future UI log hygiene.
    // last_engine_log: Option<Instant>,
    // Strict paused behavior toggle (UI)
    strict_pause: bool,
    // Track when a paused seek was requested (for overlay timing)
    last_seek_request_at: Option<Instant>,
    // Last presented frame PTS for current source (path, pts seconds)
    last_present_pts: Option<(String, f64)>,
    preview_last_capture: Option<PreviewCapture>,
    // User settings
    settings: PreviewSettings,
    show_settings: bool,
    // Screenplay assistant state
    show_screenplay_panel: bool,
    screenplay_api_token: String,
    screenplay_provider: crate::screenplay::ProviderKind,
    screenplay_model: String,
    screenplay_active_tab: ScreenplayTab,
    screenplay_session: Option<crate::screenplay::ScreenplaySession>,
    screenplay_questions: Vec<crate::screenplay::ScreenplayQuestion>,
    screenplay_input: String,
    screenplay_revision_input: String,
    screenplay_revision_scope: crate::screenplay::RevisionScope,
    screenplay_error: Option<String>,
    screenplay_logs: std::collections::VecDeque<String>,
    screenplay_busy: bool,
    screenplay_generate_busy: bool,
    screenplay_cancel_requested: bool,
    screenplay_session_handle:
        Option<std::sync::Arc<std::sync::Mutex<crate::screenplay::ScreenplaySessionHandle>>>,
    screenplay_event_tx: Sender<crate::screenplay::ScreenplayEvent>,
    screenplay_event_rx: Receiver<crate::screenplay::ScreenplayEvent>,
    // ComfyUI integration (Phase 1)
    comfy: crate::comfyui::ComfyUiManager,
    show_comfy_panel: bool,
    // Editable input for ComfyUI repo path (separate from committed config)
    comfy_repo_input: String,
    // Installer UI state
    comfy_install_dir_input: String,
    comfy_torch_backend: crate::comfyui::TorchBackend,
    comfy_venv_python_input: String,
    comfy_recreate_venv: bool,
    comfy_install_ffmpeg: bool,
    // Remote/Local ComfyUI job monitor
    comfy_ws_monitor: bool,
    comfy_ws_thread: Option<std::thread::JoinHandle<()>>,
    comfy_ws_stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    comfy_api_key: String,
    // Modal cloud job submission
    modal_enabled: bool,
    modal_base_url: String,
    modal_api_key: String,
    modal_payload: String,
    modal_logs: std::collections::VecDeque<String>,
    modal_rx: Receiver<ModalEvent>,
    modal_tx: Sender<ModalEvent>,
    // Cached recent jobs/artifacts from /healthz
    modal_recent: Vec<(String, Vec<(String, String)>)>,
    // Cloud (Modal) live monitor
    cloud_target: CloudTarget,
    // Optional cloud relay (WS/SSE over WS) endpoint for progress + artifacts
    modal_relay_ws_url: String,
    // Live cloud monitor state
    modal_queue_pending: usize,
    modal_queue_running: usize,
    modal_job_progress: std::collections::HashMap<String, (f32, u32, u32, std::time::Instant)>,
    modal_job_source: std::collections::HashMap<String, crate::CloudUpdateSrc>,
    modal_phase_plans: std::collections::HashMap<String, PhasePlan>,
    modal_phase_agg: std::collections::HashMap<String, PhaseAgg>,
    // Active job id; only this job's progress is shown/imported
    modal_active_job: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    // Track expected unique filename prefixes per job (for artifact filtering)
    modal_job_prefixes: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
    // Cloud monitor lifecycle
    modal_monitor_requested: bool,
    modal_last_progress_at: Option<Instant>,
    // Known jobs queued locally this session
    modal_known_jobs: std::collections::HashSet<String>,
    pip_index_url_input: String,
    pip_extra_index_url_input: String,
    pip_trusted_hosts_input: String,
    pip_proxy_input: String,
    pip_no_cache: bool,
    // Embedded ComfyUI webview
    comfy_embed_inside: bool,
    #[allow(dead_code)]
    comfy_webview: Option<Box<dyn crate::embed_webview::WebViewHost>>,
    comfy_embed_logs: std::collections::VecDeque<String>,
    // Placement and sizing for embedded view
    comfy_embed_in_assets: bool,
    comfy_assets_height: f32,
    // Floating ComfyUI panel window visibility
    show_comfy_view_window: bool,
    // Auto-import from ComfyUI outputs
    comfy_auto_import: bool,
    comfy_import_logs: std::collections::VecDeque<String>,
    comfy_client_id: String,
    comfy_jobs: std::collections::HashMap<String, ComfyJobInfo>,
    comfy_known_prompts: std::collections::HashSet<String>,
    comfy_queue_pending: usize,
    comfy_queue_running: usize,
    comfy_last_queue_poll: Option<Instant>,
    comfy_ws_rx: Receiver<ComfyWsEvent>,
    comfy_ws_tx: Sender<ComfyWsEvent>,
    comfy_http_agent: ureq::Agent,
    comfy_storyboard_jobs: std::collections::HashMap<uuid::Uuid, ComfyStoryboardJob>,
    comfy_prompt_to_card: std::collections::HashMap<String, uuid::Uuid>,
    comfy_alerts: std::collections::VecDeque<ComfyAlert>,
    #[cfg(not(target_arch = "wasm32"))]
    raw_window_handle: Option<RawWindowHandle>,
    #[cfg(not(target_arch = "wasm32"))]
    raw_display_handle: Option<RawDisplayHandle>,
    comfy_ingest_thread: Option<std::thread::JoinHandle<()>>,
    comfy_ingest_stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    comfy_ingest_rx: Receiver<(String, std::path::PathBuf)>,
    comfy_ingest_tx: Sender<(String, std::path::PathBuf)>,
    // Project id that the ingest thread is currently bound to (for routing)
    comfy_ingest_project_id: Option<String>,
    // Projects page
    show_projects: bool,
    new_project_name: String,
    new_project_base: String,
    // App mode: show project picker before opening editor
    mode: AppMode,
    workspace_view: WorkspaceView,
    chat_messages: Vec<ChatMessage>,
    chat_input: String,
    chat_busy: bool,
    chat_error: Option<String>,
    chat_model: String,
    chat_base_url: String,
    chat_system_prompt: String,
    chat_temperature: f32,
    chat_event_tx: Sender<ChatEvent>,
    chat_event_rx: Receiver<ChatEvent>,
    storyboard_cards: Vec<StoryboardCard>,
    storyboard_selected: Option<usize>,
    storyboard_previews: std::collections::HashMap<uuid::Uuid, egui::TextureHandle>,
    storyboard_input_previews: std::collections::HashMap<(uuid::Uuid, String), egui::TextureHandle>,
    storyboard_preview_resets: std::collections::HashSet<uuid::Uuid>,
    storyboard_pending_input_refresh:
        std::collections::HashMap<uuid::Uuid, StoryboardPendingInputRefresh>,
    storyboard_workflows: Vec<StoryboardWorkflowPreset>,
    storyboard_batch_busy: bool,
    // Autosave indicator
    last_save_at: Option<Instant>,
    // Assets UI: cached thumbnail textures
    asset_thumb_textures: std::collections::HashMap<String, egui::TextureHandle>,
    textures_to_free_next_frame: Vec<egui::TextureHandle>,
    // Dragging asset from assets panel into timeline
    dragging_asset: Option<project::AssetRow>,
    // Assets UI: adjustable thumbnail width
    asset_thumb_w: f32,
    assets_drop_rect: Option<egui::Rect>,
    timeline_drop_rect: Option<egui::Rect>,
    pending_timeline_drops: Vec<PendingTimelineDrop>,
}

struct PendingTimelineDrop {
    path: std::path::PathBuf,
    track_hint: usize,
    frame: i64,
}

impl App {
    fn handle_cache_event(&mut self, event: CacheEvent) {
        match event {
            CacheEvent::StatusChanged { id, status } => {
                match &status {
                    crate::cache::job::JobStatus::Completed(path) => {
                        tracing::info!(
                            target = "cache",
                            id = id.0,
                            output = %path.display(),
                            "optimized media ready"
                        );
                    }
                    crate::cache::job::JobStatus::Failed(msg) => {
                        tracing::warn!(
                            target = "cache",
                            id = id.0,
                            error = %msg,
                            "optimized media job failed"
                        );
                    }
                    crate::cache::job::JobStatus::Canceled => {
                        tracing::info!(target = "cache", id = id.0, "optimized media job canceled");
                    }
                    crate::cache::job::JobStatus::Queued
                    | crate::cache::job::JobStatus::InProgress(_) => {}
                }
                self.cache_job_status.insert(id, status);
            }
        }
    }

    fn handle_proxy_event(&mut self, event: crate::proxy_queue::ProxyEvent) {
        let status_clone = event.status.clone();
        self.proxy_status
            .insert(event.asset_id.clone(), status_clone);
        match event.status {
            ProxyStatus::Completed { ref proxy_path } => {
                self.append_proxy_log(
                    &event.asset_id,
                    format!("Proxy completed: {}", proxy_path.display()),
                );
                tracing::info!(asset = %event.asset_id, path = %proxy_path.display(), "proxy ready");
                self.auto_proxy_requests.remove(&event.asset_id);
                self.refresh_asset_cache_entry(&event.asset_id);
                if let Ok(asset) = self.db.get_asset(&event.asset_id) {
                    let name = std::path::Path::new(&asset.src_abs)
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| asset.id.clone());
                    self.push_comfy_alert(
                        format!("Proxy ready: {}", name),
                        ComfyAlertKind::Success,
                        Duration::from_secs(4),
                    );
                    self.asset_cache.insert(
                        asset.src_abs.clone(),
                        CachedAssetEntry {
                            asset,
                            last_refresh: Instant::now(),
                        },
                    );
                }
            }
            ProxyStatus::Failed { ref message } => {
                self.append_proxy_log(&event.asset_id, format!("Proxy failed: {}", message));
                tracing::warn!(asset = %event.asset_id, "proxy failed: {message}");
                self.auto_proxy_requests.remove(&event.asset_id);
                self.push_comfy_alert(
                    format!("Proxy failed: {}", message),
                    ComfyAlertKind::Warning,
                    Duration::from_secs(5),
                );
            }
            ProxyStatus::Running { progress } => {
                self.append_proxy_log(
                    &event.asset_id,
                    format!(
                        "Proxy running: {:.0}%",
                        (progress * 100.0).clamp(0.0, 100.0)
                    ),
                );
            }
            ProxyStatus::Pending => {
                self.append_proxy_log(&event.asset_id, "Proxy pending".to_string());
            }
        }
    }
    fn handle_external_file_drops(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        if dropped.is_empty() {
            return;
        }

        let drop_pos = ctx.input(|i| i.pointer.interact_pos().or_else(|| i.pointer.hover_pos()));

        for file in dropped {
            let Some(path_buf) = file.path else { continue };
            let path = path_buf.canonicalize().unwrap_or(path_buf.clone());

            let mut handled = false;

            if let Some(pos) = drop_pos {
                if !handled {
                    if let Some(rect) = self.timeline_drop_rect {
                        if rect.contains(pos) {
                            if let Some((track, frame)) = self.timeline_drop_target(pos) {
                                let _ = self.import_files(&[path.clone()]);
                                self.pending_timeline_drops.push(PendingTimelineDrop {
                                    path: path.clone(),
                                    track_hint: track,
                                    frame,
                                });
                                handled = true;
                            }
                        }
                    }
                }
                if !handled {
                    if let Some(rect) = self.assets_drop_rect {
                        if rect.contains(pos) {
                            let _ = self.import_files(&[path.clone()]);
                            handled = true;
                        }
                    }
                }
            }

            if !handled {
                let _ = self.import_files(&[path.clone()]);
            }
        }
    }

    fn process_pending_timeline_drops(&mut self) {
        let mut remaining = Vec::new();
        let pending_items = std::mem::take(&mut self.pending_timeline_drops);
        for pending in pending_items {
            let path_str = pending.path.to_string_lossy().to_string();
            match self.db.find_asset_by_path(&self.project_id, &path_str) {
                Ok(Some(asset)) => {
                    self.insert_asset_at(&asset, pending.track_hint, pending.frame);
                }
                Ok(None) | Err(_) => {
                    remaining.push(pending);
                }
            }
        }
        self.pending_timeline_drops = remaining;
    }

    fn selected_asset_id(&self) -> Option<String> {
        let (track_idx, item_idx) = self.selected?;
        let binding = self.seq.graph.tracks.get(track_idx)?;
        let node_id = binding.node_ids.get(item_idx)?;
        let node = self.seq.graph.nodes.get(node_id)?;
        match &node.kind {
            timeline_crate::TimelineNodeKind::Clip(_) => node.label.clone(),
            _ => None,
        }
    }

    fn timeline_drop_target(&self, pos: egui::Pos2) -> Option<(usize, i64)> {
        let rect = self.timeline_drop_rect?;
        let track_count = self.seq.graph.tracks.len();
        if track_count == 0 {
            return None;
        }
        let track_h = 48.0;
        let mut track = ((pos.y - rect.top()) / track_h).floor() as isize;
        track = track.clamp(0, track_count as isize - 1);
        let local_x = (pos.x - rect.left()).max(0.0) as f64;
        let zoom = self.zoom_px_per_frame.max(0.001) as f64;
        let frame = (local_x / zoom).round() as i64;
        Some((track as usize, frame.max(0)))
    }

    fn collect_modal_artifacts(
        candidates: &mut Vec<(String, String)>,
        arr_opt: Option<Vec<serde_json::Value>>,
    ) {
        if let Some(items) = arr_opt {
            for it in items {
                let name = it.get("filename").and_then(|s| s.as_str()).unwrap_or("");
                let url = it.get("url").and_then(|s| s.as_str()).unwrap_or("");
                if url.is_empty() {
                    continue;
                }
                if !name.to_ascii_lowercase().ends_with(".mp4") {
                    continue;
                }
                candidates.push((name.to_string(), url.to_string()));
            }
        }
    }

    fn switch_workspace(&mut self, view: WorkspaceView) {
        let previous = self.workspace_view;
        if previous == view {
            return;
        }
        if matches!(view, WorkspaceView::Chat | WorkspaceView::Storyboard) {
            self.pause_timeline_playback();
        }
        if matches!(previous, WorkspaceView::Timeline)
            && !matches!(view, WorkspaceView::Timeline)
            && self.comfy_embed_inside
        {
            self.close_comfy_embed_host("Embedded view closed (workspace change)");
        }
        self.workspace_view = view;
    }

    fn pause_timeline_playback(&mut self) {
        if !self.playback_clock.playing {
            return;
        }
        let fps_num = self.seq.fps.num.max(1) as f64;
        let fps_den = self.seq.fps.den.max(1) as f64;
        let seq_fps = fps_num / fps_den;
        let current_sec = if seq_fps > 0.0 {
            (self.playhead as f64) / seq_fps
        } else {
            0.0
        };
        self.playback_clock.pause(current_sec);
        self.engine.state = PlayState::Paused;
        if let Some(engine) = &self.audio_out {
            engine.pause(current_sec);
        }
    }

    fn chat_reset(&mut self) {
        self.chat_messages.clear();
        self.chat_input.clear();
        self.chat_error = None;
        self.chat_busy = false;
    }

    fn chat_send_current(&mut self) {
        if self.chat_busy {
            return;
        }
        let trimmed = self.chat_input.trim();
        if trimmed.is_empty() {
            return;
        }
        let user_message = ChatMessage {
            role: ChatRole::User,
            text: trimmed.to_string(),
        };
        self.chat_messages.push(user_message);
        self.trim_chat_history();
        self.chat_input.clear();
        self.chat_busy = true;
        self.chat_error = None;

        let history = self.chat_messages.clone();
        let system_prompt = self.chat_system_prompt.clone();
        let base_url = self.chat_base_url.clone();
        let model = self.chat_model.clone();
        let temperature = self.chat_temperature;
        let tx = self.chat_event_tx.clone();
        std::thread::spawn(move || {
            let prompt = App::build_chat_prompt(&system_prompt, &history);
            let result = App::call_local_llm(&base_url, &model, &prompt, temperature);
            let _ = tx.send(ChatEvent::Response(result));
        });
    }

    fn chat_handle_event(&mut self, ev: ChatEvent) {
        match ev {
            ChatEvent::Response(res) => {
                self.chat_busy = false;
                match res {
                    Ok(text) => {
                        let trimmed = text.trim();
                        if !trimmed.is_empty() {
                            self.chat_error = None;
                            self.chat_messages.push(ChatMessage {
                                role: ChatRole::Assistant,
                                text: trimmed.to_string(),
                            });
                            self.trim_chat_history();
                        }
                    }
                    Err(err) => {
                        self.chat_error = Some(err.to_string());
                    }
                }
            }
        }
    }

    fn trim_chat_history(&mut self) {
        if self.chat_messages.len() <= CHAT_HISTORY_LIMIT {
            return;
        }
        let excess = self.chat_messages.len() - CHAT_HISTORY_LIMIT;
        if matches!(
            self.chat_messages.first().map(|msg| msg.role),
            Some(ChatRole::System)
        ) {
            let available = self.chat_messages.len().saturating_sub(1);
            let drain = excess.min(available);
            if drain > 0 {
                self.chat_messages.drain(1..1 + drain);
            }
        } else {
            self.chat_messages.drain(0..excess);
        }
    }

    fn build_chat_prompt(system_prompt: &str, history: &[ChatMessage]) -> String {
        let mut prompt = String::new();
        let sys = system_prompt.trim();
        if !sys.is_empty() {
            prompt.push_str(sys);
            if !sys.ends_with('\n') {
                prompt.push('\n');
            }
        }
        let start = history.len().saturating_sub(CHAT_PROMPT_WINDOW);
        for message in history.iter().skip(start) {
            let role = match message.role {
                ChatRole::System => "System",
                ChatRole::User => "User",
                ChatRole::Assistant => "Assistant",
            };
            prompt.push_str(role);
            prompt.push_str(": ");
            prompt.push_str(message.text.trim());
            prompt.push('\n');
        }
        prompt.push_str("Assistant:");
        prompt
    }

    fn call_local_llm(
        base_url: &str,
        model: &str,
        prompt: &str,
        temperature: f32,
    ) -> Result<String> {
        let model = {
            let trimmed = model.trim();
            if trimmed.is_empty() {
                "llama3.2:latest"
            } else {
                trimmed
            }
        };
        let base = {
            let trimmed = base_url.trim();
            if trimmed.is_empty() {
                "http://localhost:11434"
            } else {
                trimmed
            }
        };
        let endpoint = if base.ends_with("/api/generate") {
            base.to_string()
        } else {
            format!("{}/api/generate", base.trim_end_matches('/'))
        };

        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout_read(Duration::from_secs(120))
            .timeout_write(Duration::from_secs(45))
            .build();
        let payload = json!({
            "model": model,
            "prompt": prompt,
            "stream": false,
            "options": {
                "temperature": temperature.clamp(0.0, 2.0),
                "num_predict": 1024,
            }
        });
        let response = agent
            .post(&endpoint)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json")
            .send_string(&payload.to_string());
        let body = match response {
            Ok(resp) => resp
                .into_string()
                .context("Failed to read response from Ollama")?,
            Err(ureq::Error::Status(code, resp)) => {
                let text = resp.into_string().unwrap_or_default();
                return Err(anyhow!("Ollama {code}: {text}"));
            }
            Err(err) => return Err(anyhow!("Failed to reach Ollama: {err}")),
        };
        let parsed: Value = serde_json::from_str(&body)
            .with_context(|| format!("Ollama returned invalid JSON: {body}"))?;
        if let Some(err) = parsed.get("error").and_then(|v| v.as_str()) {
            return Err(anyhow!(err.to_string()));
        }
        if let Some(text) = parsed
            .get("response")
            .and_then(|v| v.as_str())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            return Ok(text.to_string());
        }
        if let Some(done) = parsed.get("done").and_then(|v| v.as_bool()) {
            if done {
                return Ok(String::new());
            }
        }
        Err(anyhow!("Ollama response missing expected content: {body}"))
    }

    fn storyboard_add_card(&mut self) -> usize {
        let default_video = self
            .storyboard_workflows
            .first()
            .and_then(|preset| preset.video_defaults.clone());
        let default_workflow_id = self.storyboard_workflows.first().map(|p| p.id);
        let preset_clone = default_workflow_id.and_then(|wid| {
            self.storyboard_workflows
                .iter()
                .find(|p| p.id == wid)
                .cloned()
        });
        let default_output_kind = preset_clone
            .as_ref()
            .map(|preset| preset.output_kind)
            .unwrap_or_default();
        let default_inputs = default_workflow_id
            .and_then(|wid| self.storyboard_workflow_input_defaults(wid))
            .unwrap_or_default();
        self.storyboard_cards.push(StoryboardCard {
            id: uuid::Uuid::new_v4(),
            title: format!("Shot {}", self.storyboard_cards.len() + 1),
            description: String::new(),
            reference_path: String::new(),
            duration_seconds: 5.0,
            preview_error: None,
            workflow_id: default_workflow_id,
            workflow_error: None,
            workflow_status: None,
            video_settings: default_video,
            output_kind: default_output_kind,
            workflow_inputs: default_inputs,
            workflow_input_errors: std::collections::HashMap::new(),
        });
        let idx = self.storyboard_cards.len() - 1;
        if let Some(card) = self.storyboard_cards.get_mut(idx) {
            Self::sync_storyboard_inputs(card, preset_clone.as_ref());
        }
        self.storyboard_selected = Some(idx);
        idx
    }

    fn storyboard_remove_selected(&mut self) -> bool {
        if let Some(idx) = self.storyboard_selected {
            if idx < self.storyboard_cards.len() {
                let card_id = self.storyboard_cards[idx].id;
                self.storyboard_cards.remove(idx);
                self.storyboard_previews.remove(&card_id);
                self.storyboard_input_previews
                    .retain(|(id, _), _| *id != card_id);
                self.remove_storyboard_job(&card_id);
                if self.storyboard_cards.is_empty() {
                    self.storyboard_selected = None;
                } else if idx >= self.storyboard_cards.len() {
                    self.storyboard_selected = Some(self.storyboard_cards.len() - 1);
                } else {
                    self.storyboard_selected = Some(idx);
                }
                if let Err(err) = self.persist_storyboard_to_settings() {
                    tracing::warn!("Failed to persist storyboard after removing card: {}", err);
                }
                return true;
            }
        }
        false
    }

    fn storyboard_move_selected(&mut self, delta: isize) {
        if let Some(idx) = self.storyboard_selected {
            let len = self.storyboard_cards.len();
            if len == 0 {
                return;
            }
            let new_idx = idx as isize + delta;
            if new_idx < 0 || new_idx >= len as isize {
                return;
            }
            self.storyboard_cards.swap(idx, new_idx as usize);
            self.storyboard_selected = Some(new_idx as usize);
        }
    }

    fn storyboard_duplicate_selected(&mut self) -> Option<usize> {
        let idx = self.storyboard_selected?;
        let card = self.storyboard_cards.get(idx)?.clone();
        let mut duplicate = card;
        duplicate.id = uuid::Uuid::new_v4();
        duplicate.title = format!("{} (Copy)", duplicate.title);
        duplicate.preview_error = None;
        duplicate.workflow_error = None;
        duplicate.workflow_input_errors.clear();
        let preset_clone = duplicate.workflow_id.and_then(|wid| {
            self.storyboard_workflows
                .iter()
                .find(|p| p.id == wid)
                .cloned()
        });
        if let Some(preset) = preset_clone.as_ref() {
            Self::sync_storyboard_inputs_with_transfer(&mut duplicate, preset, Some(preset));
        } else {
            Self::sync_storyboard_inputs(&mut duplicate, None);
        }
        let insert_at = std::cmp::min(idx + 1, self.storyboard_cards.len());
        self.storyboard_cards.insert(insert_at, duplicate);
        self.storyboard_selected = Some(insert_at);
        Some(insert_at)
    }

    fn storyboard_workflow_defaults(
        &self,
        workflow_id: uuid::Uuid,
    ) -> Option<StoryboardVideoSettings> {
        self.storyboard_workflows
            .iter()
            .find(|preset| preset.id == workflow_id)
            .and_then(|preset| preset.video_defaults.clone())
    }

    fn storyboard_workflow_input_specs(
        &self,
        workflow_id: uuid::Uuid,
    ) -> Option<&[StoryboardWorkflowInputSpec]> {
        self.storyboard_workflows
            .iter()
            .find(|preset| preset.id == workflow_id)
            .map(|preset| preset.input_specs.as_slice())
    }

    fn storyboard_workflow_input_defaults(
        &self,
        workflow_id: uuid::Uuid,
    ) -> Option<std::collections::HashMap<String, StoryboardInputValue>> {
        self.storyboard_workflows
            .iter()
            .find(|preset| preset.id == workflow_id)
            .map(|preset| {
                preset
                    .input_specs
                    .iter()
                    .filter_map(|spec| {
                        spec.default_value
                            .clone()
                            .map(|value| (spec.map_key.clone(), value))
                    })
                    .collect()
            })
    }

    fn storyboard_workflow_slug(preset: &StoryboardWorkflowPreset) -> String {
        Self::slugify_workflow_name(&preset.name, preset.id)
    }

    fn slugify_workflow_name(name: &str, id: uuid::Uuid) -> String {
        let mut slug = String::new();
        let mut last_dash = false;
        for ch in name.chars() {
            let lower = ch.to_ascii_lowercase();
            if lower.is_ascii_alphanumeric() {
                slug.push(lower);
                last_dash = false;
            } else if !last_dash {
                slug.push('-');
                last_dash = true;
            }
        }
        if slug.is_empty() {
            slug.push_str("workflow");
        }
        if slug.ends_with('-') {
            slug.pop();
        }
        let short_id = id.as_simple().to_string();
        let short = &short_id[..8.min(short_id.len())];
        format!("{}-{}", slug, short)
    }

    fn normalized_workflow_key(raw: &str) -> String {
        raw.trim().to_ascii_lowercase()
    }

    fn is_autofilled_workflow_input(spec: &StoryboardWorkflowInputSpec) -> bool {
        let key = spec.input_key.to_ascii_lowercase();
        if spec
            .node_class
            .eq_ignore_ascii_case("RunwayImageToVideoNodeGen4")
            && matches!(key.as_str(), "duration" | "seed")
        {
            return true;
        }
        spec.node_class.eq_ignore_ascii_case("LoadImage")
            && spec.input_key.eq_ignore_ascii_case("image")
    }

    fn workflow_input_kind_label(kind: &StoryboardWorkflowInputKind) -> &'static str {
        match kind {
            StoryboardWorkflowInputKind::Text { multiline: true } => "multiline text",
            StoryboardWorkflowInputKind::Text { multiline: false } => "text",
            StoryboardWorkflowInputKind::File => "file path",
            StoryboardWorkflowInputKind::Integer => "integer",
            StoryboardWorkflowInputKind::Float => "number",
            StoryboardWorkflowInputKind::Boolean => "boolean",
            StoryboardWorkflowInputKind::Array => "array",
            StoryboardWorkflowInputKind::Object => "object",
            StoryboardWorkflowInputKind::Null => "value",
        }
    }

    fn screenplay_workflow_context(&self) -> Option<String> {
        if self.storyboard_workflows.is_empty() {
            return None;
        }
        let mut out = String::from(
            "Storyboard workflow catalog (you must choose exactly one workflow per shot and include it under each shot's \"workflow\" object):\n",
        );
        for preset in &self.storyboard_workflows {
            let slug = Self::storyboard_workflow_slug(preset);
            let _ = writeln!(
                out,
                "- key: {} — {} ({})",
                slug,
                preset.name,
                preset.output_kind.label()
            );
            let mut inputs: Vec<String> = preset
                .input_specs
                .iter()
                .filter(|spec| !Self::is_autofilled_workflow_input(spec))
                .map(|spec| {
                    format!(
                        "{} [{}]",
                        spec.input_key,
                        Self::workflow_input_kind_label(&spec.kind)
                    )
                })
                .collect();
            inputs.sort();
            if inputs.is_empty() {
                let _ = writeln!(out, "  Inputs: (no manual inputs required)");
            } else {
                let _ = writeln!(out, "  Inputs: {}", inputs.join(", "));
            }
        }
        out.push_str(
            "\nEvery shot's JSON block must include:\n\
\"workflow\": {\"key\": \"<workflow-key>\", \"inputs\": {\"input_key\": value}}\n\
Use one of the keys listed above. Populate the \"inputs\" object with values for that workflow's manual inputs (e.g., prompt, ratio).",
        );
        Some(out)
    }

    fn sync_storyboard_inputs(
        card: &mut StoryboardCard,
        preset: Option<&StoryboardWorkflowPreset>,
    ) -> bool {
        match preset {
            Some(preset) => {
                let mut new_inputs = std::collections::HashMap::new();
                for spec in &preset.input_specs {
                    let managed = Self::is_managed_storyboard_input(spec);
                    let value = if managed {
                        card.workflow_inputs
                            .get(&spec.map_key)
                            .cloned()
                            .or_else(|| {
                                spec.default_value.clone().or_else(|| {
                                    Some(StoryboardInputValue::default_for_kind(&spec.kind))
                                })
                            })
                            .unwrap_or_else(|| StoryboardInputValue::default_for_kind(&spec.kind))
                    } else {
                        match card.workflow_inputs.get(&spec.map_key) {
                            Some(existing) if existing.matches_kind(&spec.kind) => existing.clone(),
                            _ => spec.default_value.clone().unwrap_or_else(|| {
                                StoryboardInputValue::default_for_kind(&spec.kind)
                            }),
                        }
                    };
                    new_inputs.insert(spec.map_key.clone(), value);
                    card.workflow_input_errors.remove(&spec.map_key);
                }
                let changed = new_inputs != card.workflow_inputs;
                if changed {
                    card.workflow_inputs = new_inputs;
                }
                card.workflow_input_errors
                    .retain(|key, _| card.workflow_inputs.contains_key(key));
                changed
            }
            None => {
                let had_inputs = !card.workflow_inputs.is_empty();
                let had_errors = !card.workflow_input_errors.is_empty();
                if had_inputs {
                    card.workflow_inputs.clear();
                }
                if had_errors {
                    card.workflow_input_errors.clear();
                }
                had_inputs || had_errors
            }
        }
    }

    fn sync_storyboard_inputs_with_transfer(
        card: &mut StoryboardCard,
        new_preset: &StoryboardWorkflowPreset,
        old_preset: Option<&StoryboardWorkflowPreset>,
    ) -> bool {
        let mut old_values = std::collections::HashMap::new();
        if let Some(old) = old_preset {
            for spec in &old.input_specs {
                if let Some(value) = card.workflow_inputs.get(&spec.map_key) {
                    old_values.insert(spec.map_key.clone(), value.clone());
                }
            }
        }
        let changed = Self::sync_storyboard_inputs(card, Some(new_preset));
        if old_values.is_empty() {
            return changed;
        }
        let mut transfers = std::collections::HashMap::new();
        for spec in &new_preset.input_specs {
            if let Some(value) = old_values
                .get(&spec.map_key)
                .filter(|value| value.matches_kind(&spec.kind))
            {
                transfers.insert(spec.map_key.clone(), value.clone());
                continue;
            }
            if let Some(source_key) = Self::find_transfer_source(spec, old_preset, &old_values) {
                if let Some(value) = old_values
                    .get(&source_key)
                    .filter(|value| value.matches_kind(&spec.kind))
                {
                    transfers.insert(spec.map_key.clone(), value.clone());
                }
            }
        }
        let mut any_transfer = false;
        for (map_key, value) in transfers {
            card.workflow_inputs.insert(map_key.clone(), value.clone());
            card.workflow_input_errors.remove(&map_key);
            any_transfer = true;
        }
        changed || any_transfer
    }

    fn find_transfer_source(
        target_spec: &StoryboardWorkflowInputSpec,
        old_preset: Option<&StoryboardWorkflowPreset>,
        old_values: &std::collections::HashMap<String, StoryboardInputValue>,
    ) -> Option<String> {
        let old = old_preset?;
        let mut best_match: Option<String> = None;
        for spec in &old.input_specs {
            if !old_values.contains_key(&spec.map_key) {
                continue;
            }
            if spec.semantic == target_spec.semantic && spec.semantic != WorkflowInputSemantic::Other
            {
                return Some(spec.map_key.clone());
            }
            if best_match.is_none()
                && spec.semantic == WorkflowInputSemantic::PromptPositive
                && target_spec.semantic == WorkflowInputSemantic::PromptSingle
            {
                best_match = Some(spec.map_key.clone());
            }
            if best_match.is_none()
                && spec.semantic == WorkflowInputSemantic::PromptSingle
                && matches!(
                    target_spec.semantic,
                    WorkflowInputSemantic::PromptPositive | WorkflowInputSemantic::PromptNegative
                )
            {
                best_match = Some(spec.map_key.clone());
            }
            if best_match.is_none()
                && matches!(
                    (spec.semantic, target_spec.semantic),
                    (WorkflowInputSemantic::Ratio, WorkflowInputSemantic::AspectRatio)
                        | (WorkflowInputSemantic::AspectRatio, WorkflowInputSemantic::Ratio)
                )
            {
                best_match = Some(spec.map_key.clone());
            }
        }
        best_match
    }

    fn is_managed_storyboard_input(spec: &StoryboardWorkflowInputSpec) -> bool {
        if spec
            .node_class
            .eq_ignore_ascii_case("RunwayImageToVideoNodeGen4")
        {
            let key = spec.input_key.to_ascii_lowercase();
            return matches!(key.as_str(), "duration" | "ratio");
        }
        spec.node_class.eq_ignore_ascii_case("LoadImage")
            && spec.input_key.eq_ignore_ascii_case("image")
    }

    fn storyboard_workflow_dir() -> std::path::PathBuf {
        project::app_data_dir().join("workflows").join("storyboard")
    }

    fn ensure_default_storyboard_workflow_file() -> std::io::Result<()> {
        let dir = Self::storyboard_workflow_dir();
        std::fs::create_dir_all(&dir)?;
        let dest = dir.join(DEFAULT_STORYBOARD_WORKFLOW_FILE);
        if !dest.exists() {
            std::fs::write(&dest, DEFAULT_STORYBOARD_WORKFLOW_JSON)?;
        }
        Ok(())
    }

    fn scan_storyboard_workflows() -> Vec<StoryboardWorkflowPreset> {
        let dir = Self::storyboard_workflow_dir();
        let mut presets = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|s| s.eq_ignore_ascii_case("json"))
                    .unwrap_or(false)
                {
                    let file_name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or_default();
                    let builtin = file_name == DEFAULT_STORYBOARD_WORKFLOW_FILE;
                    if let Some(preset) = StoryboardWorkflowPreset::from_path(path.clone(), builtin)
                    {
                        presets.push(preset);
                    }
                }
            }
        }
        presets
    }

    pub(crate) fn refresh_storyboard_workflows(&mut self) {
        if let Err(e) = Self::ensure_default_storyboard_workflow_file() {
            eprintln!("Failed to ensure default storyboard workflow: {}", e);
        }
        let mut presets = Self::scan_storyboard_workflows();
        presets.sort_by(|a, b| match (a.builtin, b.builtin) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        });
        self.storyboard_workflows = presets;
        self.ensure_storyboard_card_workflow_selection();
    }

    pub(crate) fn load_storyboard_from_settings(&mut self) {
        match self.db.get_project_settings_json(&self.project_id) {
            Ok(settings) => {
                if let Some(value) = settings.get("storyboard") {
                    match serde_json::from_value::<PersistedStoryboard>(value.clone()) {
                        Ok(stored) => {
                            self.storyboard_cards = stored.cards;
                            self.storyboard_selected = stored.selected.and_then(|idx| {
                                if idx < self.storyboard_cards.len() {
                                    Some(idx)
                                } else {
                                    None
                                }
                            });
                            let active_ids: std::collections::HashSet<uuid::Uuid> =
                                self.storyboard_cards.iter().map(|card| card.id).collect();
                            self.clear_all_storyboard_jobs();
                            for (card_id, persisted_job) in stored.comfy_jobs {
                                if !active_ids.contains(&card_id) {
                                    continue;
                                }
                                let job: ComfyStoryboardJob = persisted_job.into();
                                if let Some(prompt_id) = job.prompt_id.clone() {
                                    self.comfy_prompt_to_card.insert(prompt_id.clone(), card_id);
                                    self.comfy_known_prompts.insert(prompt_id);
                                }
                                self.comfy_storyboard_jobs.insert(card_id, job);
                            }
                        }
                        Err(err) => {
                            eprintln!("Failed to parse storyboard settings: {}", err);
                            self.storyboard_cards.clear();
                            self.storyboard_selected = None;
                            self.clear_all_storyboard_jobs();
                        }
                    }
                } else {
                    self.storyboard_cards.clear();
                    self.storyboard_selected = None;
                    self.clear_all_storyboard_jobs();
                }
            }
            Err(err) => {
                eprintln!("Failed to load project settings: {}", err);
                self.storyboard_cards.clear();
                self.storyboard_selected = None;
                self.clear_all_storyboard_jobs();
            }
        }
        self.storyboard_previews.clear();
        self.storyboard_input_previews.clear();
        self.storyboard_pending_input_refresh.clear();
        self.ensure_storyboard_card_workflow_selection();
        if self.storyboard_cards.is_empty() {
            self.storyboard_selected = None;
        } else if self
            .storyboard_selected
            .map(|idx| idx >= self.storyboard_cards.len())
            .unwrap_or(true)
        {
            self.storyboard_selected = Some(0);
        }
    }

    pub(crate) fn persist_storyboard_to_settings(&mut self) -> anyhow::Result<()> {
        self.cleanup_storyboard_jobs();
        let stored = PersistedStoryboard {
            cards: self.storyboard_cards.clone(),
            selected: self.storyboard_selected,
            comfy_jobs: self
                .comfy_storyboard_jobs
                .iter()
                .map(|(card_id, job)| (*card_id, job.into()))
                .collect(),
        };
        let mut settings = self.db.get_project_settings_json(&self.project_id)?;
        let mut map = match settings {
            serde_json::Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        map.insert("storyboard".to_string(), serde_json::to_value(stored)?);
        let value = serde_json::Value::Object(map);
        self.db
            .update_project_settings_json(&self.project_id, &value)?;
        Ok(())
    }

    pub(crate) fn storyboard_replace_with_screenplay_draft(
        &mut self,
        draft: &crate::screenplay::ScreenplayDraft,
    ) -> usize {
        self.storyboard_cards.clear();
        self.storyboard_selected = None;
        self.storyboard_previews.clear();
        self.storyboard_input_previews.clear();
        self.storyboard_pending_input_refresh.clear();
        self.clear_all_storyboard_jobs();

        let preset_lookup: std::collections::HashMap<uuid::Uuid, StoryboardWorkflowPreset> = self
            .storyboard_workflows
            .iter()
            .map(|preset| (preset.id, preset.clone()))
            .collect();
        let workflow_slug_map: std::collections::HashMap<String, uuid::Uuid> = self
            .storyboard_workflows
            .iter()
            .map(|preset| {
                (
                    Self::normalized_workflow_key(&Self::storyboard_workflow_slug(preset)),
                    preset.id,
                )
            })
            .collect();
        let workflow_input_lookup: std::collections::HashMap<
            uuid::Uuid,
            std::collections::HashMap<String, usize>,
        > = self
            .storyboard_workflows
            .iter()
            .map(|preset| {
                let mut map = std::collections::HashMap::new();
                for (idx, spec) in preset.input_specs.iter().enumerate() {
                    map.insert(spec.input_key.to_ascii_lowercase(), idx);
                    map.insert(spec.map_key.to_ascii_lowercase(), idx);
                }
                (preset.id, map)
            })
            .collect();
        let prompt_key_map: std::collections::HashMap<
            uuid::Uuid,
            (Option<String>, Option<String>),
        > = self
            .storyboard_workflows
            .iter()
            .map(|preset| {
                let mut positive = None;
                let mut negative = None;
                for spec in &preset.input_specs {
                    let label_lower = spec.label.to_ascii_lowercase();
                    if positive.is_none() && label_lower.contains("positive prompt") {
                        positive = Some(spec.map_key.clone());
                    }
                    if negative.is_none() && label_lower.contains("negative prompt") {
                        negative = Some(spec.map_key.clone());
                    }
                    if positive.is_some() && negative.is_some() {
                        break;
                    }
                }
                (preset.id, (positive, negative))
            })
            .collect();

        for (idx, shot) in draft.shots.iter().enumerate() {
            let card_index = self.storyboard_add_card();
            if let Some(card) = self.storyboard_cards.get_mut(card_index) {
                card.title = if shot.title.trim().is_empty() {
                    format!("Shot {}", idx + 1)
                } else {
                    shot.title.clone()
                };
                let description = if !shot.visual_description.trim().is_empty() {
                    shot.visual_description.clone()
                } else if !shot.action.trim().is_empty() {
                    shot.action.clone()
                } else if !shot.prompt.trim().is_empty() {
                    shot.prompt.clone()
                } else {
                    String::new()
                };
                card.description = description;
                card.duration_seconds = shot.duration.max(0.1);

                let mut desired_workflow = shot
                    .workflow_id
                    .and_then(|wid| preset_lookup.get(&wid).map(|_| wid));
                if desired_workflow.is_none() {
                    if let Some(key) = shot
                        .workflow_key
                        .as_ref()
                        .map(|k| Self::normalized_workflow_key(k))
                        .filter(|k| !k.is_empty())
                    {
                        if let Some(wid) = workflow_slug_map.get(&key) {
                            desired_workflow = Some(*wid);
                        }
                    }
                }
                if let Some(workflow_id) = desired_workflow {
                    card.workflow_id = Some(workflow_id);
                    if let Some(preset) = preset_lookup.get(&workflow_id) {
                        card.video_settings = preset.video_defaults.clone();
                        card.output_kind = preset.output_kind;
                    }
                }
                if !shot.workflow_inputs.is_empty() {
                    if let Some(workflow_id) = card.workflow_id {
                        if let (Some(preset), Some(index_map)) = (
                            preset_lookup.get(&workflow_id),
                            workflow_input_lookup.get(&workflow_id),
                        ) {
                            for (input_name, raw_value) in shot.workflow_inputs.iter() {
                                let trimmed = input_name.trim();
                                if trimmed.is_empty() {
                                    continue;
                                }
                                let lookup_key = trimmed.to_ascii_lowercase();
                                let spec_idx = index_map.get(&lookup_key).copied().or_else(|| {
                                    preset.input_specs.iter().position(|spec| {
                                        spec.map_key.eq_ignore_ascii_case(trimmed)
                                            || spec.input_key.eq_ignore_ascii_case(trimmed)
                                    })
                                });
                                if let Some(idx) = spec_idx {
                                    let spec = &preset.input_specs[idx];
                                    if Self::is_autofilled_workflow_input(spec) {
                                        continue;
                                    }
                                    let value = StoryboardInputValue::from_json_with_kind(
                                        raw_value, &spec.kind,
                                    );
                                    card.workflow_inputs.insert(spec.map_key.clone(), value);
                                    card.workflow_input_errors.remove(&spec.map_key);
                                }
                            }
                        }
                    }
                }

                if let Some(workflow_id) = card.workflow_id {
                    if let Some((pos_key, neg_key)) = prompt_key_map.get(&workflow_id) {
                        if let Some(key) = pos_key.as_ref() {
                            let positive_prompt = shot.prompt.trim();
                            if !positive_prompt.is_empty()
                                && !card.workflow_inputs.contains_key(key)
                            {
                                card.workflow_inputs.insert(
                                    key.clone(),
                                    StoryboardInputValue::Text(positive_prompt.to_string()),
                                );
                                card.workflow_input_errors.remove(key);
                            }
                        }
                        if let Some(key) = neg_key.as_ref() {
                            let negative_prompt = shot.negative_prompt.trim();
                            if !negative_prompt.is_empty()
                                && !card.workflow_inputs.contains_key(key)
                            {
                                card.workflow_inputs.insert(
                                    key.clone(),
                                    StoryboardInputValue::Text(negative_prompt.to_string()),
                                );
                                card.workflow_input_errors.remove(key);
                            }
                        }
                    }
                }
            }
        }

        if self.storyboard_cards.is_empty() {
            self.storyboard_selected = None;
        } else {
            self.storyboard_selected = Some(0);
        }

        self.ensure_storyboard_card_workflow_selection();
        if let Err(err) = self.persist_storyboard_to_settings() {
            eprintln!(
                "Failed to save storyboard after applying screenplay: {}",
                err
            );
        }

        self.storyboard_cards.len()
    }

    pub(crate) fn ensure_storyboard_card_workflow_selection(&mut self) {
        let default = self.storyboard_workflows.first().map(|p| p.id);
        let preset_map: std::collections::HashMap<uuid::Uuid, StoryboardWorkflowPreset> = self
            .storyboard_workflows
            .iter()
            .map(|preset| (preset.id, preset.clone()))
            .collect();
        for card in &mut self.storyboard_cards {
            let previous_workflow = card.workflow_id;
            let previous_preset = previous_workflow.and_then(|wid| preset_map.get(&wid));
            let is_valid = card
                .workflow_id
                .map(|wid| self.storyboard_workflows.iter().any(|p| p.id == wid))
                .unwrap_or(false);
            if !is_valid {
                card.workflow_id = default;
                card.video_settings = card
                    .workflow_id
                    .and_then(|wid| preset_map.get(&wid))
                    .and_then(|preset| preset.video_defaults.clone());
            }
            let preset = card.workflow_id.and_then(|wid| preset_map.get(&wid));
            card.output_kind = preset
                .map(|p| p.output_kind)
                .unwrap_or_else(StoryboardWorkflowOutputKind::default);
            match preset {
                Some(preset) => {
                    Self::sync_storyboard_inputs_with_transfer(card, preset, previous_preset);
                }
                None => {
                    Self::sync_storyboard_inputs(card, None);
                }
            }
        }
    }

    fn storyboard_import_workflow(&mut self, path: &std::path::Path) -> Result<uuid::Uuid, String> {
        let file_name = path
            .file_name()
            .and_then(|s| Some(s.to_string_lossy().to_string()))
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "Workflow file must have a name".to_string())?;
        let dir = Self::storyboard_workflow_dir();
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("Failed to create workflow directory: {}", e))?;
        let mut dest = dir.join(&file_name);
        if dest.exists() {
            let stem = dest
                .file_stem()
                .and_then(|s| Some(s.to_string_lossy().to_string()))
                .unwrap_or_else(|| "workflow".to_string());
            let ext = dest
                .extension()
                .and_then(|e| Some(e.to_string_lossy().to_string()))
                .unwrap_or_else(|| "json".to_string());
            let mut i = 1;
            loop {
                let candidate = dir.join(format!("{}-{}.{}", stem, i, ext));
                if !candidate.exists() {
                    dest = candidate;
                    break;
                }
                i += 1;
            }
        }
        std::fs::copy(path, &dest).map_err(|e| format!("Failed to copy workflow: {}", e))?;
        self.refresh_storyboard_workflows();
        self.ensure_storyboard_card_workflow_selection();
        let canonical = dest.canonicalize().unwrap_or(dest.clone());
        let id = uuid_from_path(&canonical);
        Ok(id)
    }

    fn storyboard_remove_workflow(&mut self, workflow_id: uuid::Uuid) -> Result<(), String> {
        let preset = self
            .storyboard_workflows
            .iter()
            .find(|preset| preset.id == workflow_id)
            .cloned()
            .ok_or_else(|| "Workflow not found.".to_string())?;
        if preset.builtin {
            return Err("Built-in workflows cannot be deleted.".to_string());
        }
        std::fs::remove_file(&preset.path)
            .map_err(|e| format!("Failed to delete workflow: {}", e))?;
        let affected_cards: Vec<uuid::Uuid> = self
            .storyboard_cards
            .iter()
            .filter(|card| card.workflow_id == Some(workflow_id))
            .map(|card| card.id)
            .collect();
        let repo_path = self.comfy.config().repo_path.clone();
        if !affected_cards.is_empty() {
            let mut jobs_to_remove: Vec<uuid::Uuid> = Vec::new();
            for card in self.storyboard_cards.iter_mut() {
                if card.workflow_id == Some(workflow_id) {
                    Self::remove_managed_storyboard_reference(
                        repo_path.as_deref(),
                        &card.reference_path,
                    );
                    card.reference_path.clear();
                    card.preview_error = None;
                    card.workflow_error = None;
                    card.workflow_status = None;
                    card.workflow_inputs.clear();
                    card.workflow_input_errors.clear();
                    card.video_settings = None;
                    card.output_kind = StoryboardWorkflowOutputKind::default();
                    card.workflow_id = None;
                    jobs_to_remove.push(card.id);
                }
            }
            for card_id in &affected_cards {
                self.storyboard_mark_preview_reset(*card_id);
            }
            for card_id in jobs_to_remove {
                self.remove_storyboard_job(&card_id);
            }
        }
        self.refresh_storyboard_workflows();
        self.ensure_storyboard_card_workflow_selection();
        if !affected_cards.is_empty() {
            if let Err(err) = self.persist_storyboard_to_settings() {
                tracing::warn!(
                    "Failed to persist storyboard after workflow deletion: {}",
                    err
                );
            }
        }
        Ok(())
    }

    fn storyboard_load_preview(&mut self, ctx: &egui::Context, idx: usize) {
        if idx >= self.storyboard_cards.len() {
            return;
        }
        let (card_id, path_string) = {
            let card = &self.storyboard_cards[idx];
            (card.id, card.reference_path.clone())
        };
        let trimmed = path_string.trim();
        let error = if trimmed.is_empty() {
            self.storyboard_previews.remove(&card_id);
            Some("Select an image or video to show a preview.".to_string())
        } else {
            let path = std::path::Path::new(trimmed);
            if !path.exists() {
                self.storyboard_previews.remove(&card_id);
                Some("File not found.".to_string())
            } else if path.is_dir() {
                self.storyboard_previews.remove(&card_id);
                Some("Folders are not supported here.".to_string())
            } else {
                match Self::storyboard_color_image_from_path(path) {
                    Ok(color_image) => {
                        let tex = ctx.load_texture(
                            format!("storyboard-preview-{}", card_id),
                            color_image,
                            egui::TextureOptions::LINEAR,
                        );
                        self.storyboard_previews.insert(card_id, tex);
                        None
                    }
                    Err(msg) => {
                        self.storyboard_previews.remove(&card_id);
                        Some(msg)
                    }
                }
            }
        };

        if let Some(card) = self.storyboard_cards.get_mut(idx) {
            card.preview_error = error;
        }
    }

    fn storyboard_load_input_preview(&mut self, ctx: &egui::Context, idx: usize, map_key: &str) {
        if idx >= self.storyboard_cards.len() {
            return;
        }
        let card_id = self.storyboard_cards[idx].id;
        let key = map_key.to_string();
        let path_value = self.storyboard_cards[idx]
            .workflow_inputs
            .get(&key)
            .and_then(|value| match value {
                StoryboardInputValue::File(path) => Some(path.clone()),
                _ => None,
            });
        let mut error: Option<String> = None;
        if let Some(path_string) = path_value {
            let trimmed = path_string.trim();
            if trimmed.is_empty() {
                self.storyboard_input_previews
                    .remove(&(card_id, key.clone()));
            } else {
                let path = std::path::Path::new(trimmed);
                if !path.exists() {
                    self.storyboard_input_previews
                        .remove(&(card_id, key.clone()));
                    error = Some("File not found.".to_string());
                } else if path.is_dir() {
                    self.storyboard_input_previews
                        .remove(&(card_id, key.clone()));
                    error = Some("Folders are not supported here.".to_string());
                } else {
                    match Self::storyboard_color_image_from_path(path) {
                        Ok(color_image) => {
                            let tex_id = format!("storyboard-input-preview-{}-{}", card_id, key);
                            let tex =
                                ctx.load_texture(tex_id, color_image, egui::TextureOptions::LINEAR);
                            self.storyboard_input_previews
                                .insert((card_id, key.clone()), tex);
                        }
                        Err(msg) => {
                            self.storyboard_input_previews
                                .remove(&(card_id, key.clone()));
                            error = Some(msg);
                        }
                    }
                }
            }
        } else {
            self.storyboard_input_previews
                .remove(&(card_id, key.clone()));
        }

        if let Some(card) = self.storyboard_cards.get_mut(idx) {
            if let Some(err) = error {
                card.workflow_input_errors.insert(key, err);
            } else {
                card.workflow_input_errors.remove(&key);
            }
        }
    }

    fn storyboard_refresh_all_input_previews(&mut self, ctx: &egui::Context, idx: usize) {
        if idx >= self.storyboard_cards.len() {
            return;
        }
        let keys: Vec<String> = self.storyboard_cards[idx]
            .workflow_inputs
            .iter()
            .filter_map(|(key, value)| match value {
                StoryboardInputValue::File(_) => Some(key.clone()),
                _ => None,
            })
            .collect();
        let card_id = self.storyboard_cards[idx].id;
        let key_set: std::collections::HashSet<String> = keys.iter().cloned().collect();
        self.storyboard_input_previews
            .retain(|(cid, key), _| *cid != card_id || key_set.contains(key));
        for key in keys {
            self.storyboard_load_input_preview(ctx, idx, &key);
        }
    }

    fn storyboard_refresh_input_previews_for_keys(
        &mut self,
        ctx: &egui::Context,
        idx: usize,
        keys: std::collections::HashSet<String>,
    ) {
        if idx >= self.storyboard_cards.len() || keys.is_empty() {
            return;
        }
        for key in keys {
            self.storyboard_load_input_preview(ctx, idx, &key);
        }
    }

    fn storyboard_mark_preview_reset(&mut self, card_id: uuid::Uuid) {
        self.storyboard_preview_resets.insert(card_id);
    }

    fn take_storyboard_preview_reset(&mut self, card_id: uuid::Uuid) -> bool {
        self.storyboard_preview_resets.remove(&card_id)
    }

    fn schedule_storyboard_input_refresh(
        &mut self,
        card_id: uuid::Uuid,
        keys: Option<Vec<String>>,
    ) {
        match keys {
            Some(list) => {
                let mut filtered: Vec<String> = list
                    .into_iter()
                    .map(|key| key.trim().to_string())
                    .filter(|key| !key.is_empty())
                    .collect();
                if filtered.is_empty() {
                    return;
                }
                if let Some(entry) = self.storyboard_pending_input_refresh.get_mut(&card_id) {
                    if matches!(entry, StoryboardPendingInputRefresh::All) {
                        return;
                    }
                }
                let set = self
                    .storyboard_pending_input_refresh
                    .entry(card_id)
                    .or_insert_with(|| {
                        StoryboardPendingInputRefresh::Keys(std::collections::HashSet::new())
                    });
                if let StoryboardPendingInputRefresh::Keys(existing) = set {
                    for key in filtered.drain(..) {
                        existing.insert(key);
                    }
                }
            }
            None => {
                self.storyboard_pending_input_refresh
                    .insert(card_id, StoryboardPendingInputRefresh::All);
            }
        }
    }

    fn take_storyboard_input_refresh(
        &mut self,
        card_id: uuid::Uuid,
    ) -> Option<StoryboardPendingInputRefresh> {
        self.storyboard_pending_input_refresh.remove(&card_id)
    }

    fn remove_storyboard_job(&mut self, card_id: &uuid::Uuid) {
        if let Some(job) = self.comfy_storyboard_jobs.remove(card_id) {
            if let Some(prompt_id) = job.prompt_id {
                self.comfy_prompt_to_card.remove(&prompt_id);
                self.comfy_known_prompts.remove(&prompt_id);
            }
        }
    }

    fn cleanup_storyboard_jobs(&mut self) {
        let active: std::collections::HashSet<uuid::Uuid> =
            self.storyboard_cards.iter().map(|card| card.id).collect();
        let to_remove: Vec<uuid::Uuid> = self
            .comfy_storyboard_jobs
            .keys()
            .filter(|id| !active.contains(id))
            .cloned()
            .collect();
        for card_id in to_remove {
            self.remove_storyboard_job(&card_id);
        }
    }

    fn clear_all_storyboard_jobs(&mut self) {
        self.comfy_storyboard_jobs.clear();
        self.comfy_prompt_to_card.clear();
        self.comfy_known_prompts.clear();
    }

    fn normalize_project_base_path(path: &std::path::Path) -> std::path::PathBuf {
        fn is_date_component(s: &str) -> bool {
            let bytes = s.as_bytes();
            bytes.len() == 10
                && bytes[4] == b'-'
                && bytes[7] == b'-'
                && bytes
                    .iter()
                    .enumerate()
                    .all(|(idx, b)| matches!(idx, 4 | 7) || b.is_ascii_digit())
        }

        let mut normalized = path.to_path_buf();
        loop {
            let mut changed = false;
            let mut candidate = normalized.clone();
            if let Some(component) = candidate.file_name().and_then(|s| s.to_str()) {
                if is_date_component(component) {
                    candidate.pop();
                    if candidate
                        .file_name()
                        .and_then(|s| s.to_str())
                        .map(|s| s.eq_ignore_ascii_case("comfy"))
                        .unwrap_or(false)
                    {
                        candidate.pop();
                        if candidate
                            .file_name()
                            .and_then(|s| s.to_str())
                            .map(|s| s.eq_ignore_ascii_case("media"))
                            .unwrap_or(false)
                        {
                            candidate.pop();
                            normalized = candidate;
                            changed = true;
                        }
                    }
                } else if component.eq_ignore_ascii_case("comfy") {
                    let mut candidate = normalized.clone();
                    candidate.pop();
                    if candidate
                        .file_name()
                        .and_then(|s| s.to_str())
                        .map(|s| s.eq_ignore_ascii_case("media"))
                        .unwrap_or(false)
                    {
                        candidate.pop();
                        normalized = candidate;
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        if normalized.as_os_str().is_empty() {
            path.to_path_buf()
        } else {
            normalized
        }
    }

    fn remove_managed_storyboard_reference(repo_path: Option<&std::path::Path>, path_str: &str) {
        let trimmed = path_str.trim();
        if trimmed.is_empty() {
            return;
        }
        let repo = match repo_path {
            Some(repo) => repo,
            None => return,
        };
        let managed_dir = repo.join("input").join("storyboard");
        let path = std::path::Path::new(trimmed);
        let mut candidates = Vec::new();
        if path.is_absolute() {
            candidates.push(path.to_path_buf());
        } else {
            candidates.push(managed_dir.join(path));
        }
        for candidate in candidates {
            if candidate.starts_with(&managed_dir) {
                if let Err(err) = std::fs::remove_file(&candidate) {
                    tracing::debug!(
                        path = %candidate.display(),
                        %err,
                        "Failed to remove storyboard reference file"
                    );
                }
            }
        }
    }

    fn storyboard_color_image_from_path(
        path: &std::path::Path,
    ) -> Result<egui::ColorImage, String> {
        if path.is_dir() {
            return Err("Path points to a directory.".to_string());
        }
        if let Ok(img) = image::open(path) {
            let rgba = img.to_rgba8();
            let (w, h) = rgba.dimensions();
            if w == 0 || h == 0 {
                return Err("Image has no size.".to_string());
            }
            return Ok(egui::ColorImage::from_rgba_unmultiplied(
                [w as usize, h as usize],
                &rgba.into_raw(),
            ));
        }

        let probe =
            media_io::probe_media(path).map_err(|e| format!("Failed to probe media: {}", e))?;
        match probe.kind {
            media_io::MediaKind::Image => Err("Unsupported image format.".to_string()),
            media_io::MediaKind::Video => {
                let frame = match media_io::decode_yuv_at(path, 0.0) {
                    Ok(frame) => frame,
                    Err(err) => {
                        #[cfg(target_os = "macos")]
                        {
                            if let Some(frame) = Self::storyboard_decode_video_fallback(path, 0.0) {
                                frame
                            } else {
                                return Err(format!("Failed to decode video frame: {}", err));
                            }
                        }
                        #[cfg(not(target_os = "macos"))]
                        {
                            return Err(format!("Failed to decode video frame: {}", err));
                        }
                    }
                };

                let pixel_format = match frame.fmt {
                    media_io::YuvPixFmt::Nv12 => renderer::PixelFormat::Nv12,
                    media_io::YuvPixFmt::P010 => renderer::PixelFormat::P010,
                };
                let rgba = renderer::convert_yuv_to_rgba(
                    pixel_format,
                    renderer::ColorSpace::Rec709,
                    frame.width,
                    frame.height,
                    &frame.y,
                    &frame.uv,
                )
                .map_err(|e| format!("Failed to convert video frame: {}", e))?;
                Ok(egui::ColorImage::from_rgba_unmultiplied(
                    [frame.width as usize, frame.height as usize],
                    &rgba,
                ))
            }
            media_io::MediaKind::Audio => {
                Err("Audio files are not supported for storyboard previews.".to_string())
            }
        }
    }

    fn comfy_authorization_header(&self) -> Option<String> {
        let key = self.comfy_api_key.trim();
        if key.is_empty() {
            None
        } else {
            Some(format!("Bearer {}", key))
        }
    }

    fn storyboard_send_to_comfy(&mut self, idx: usize) -> Result<(), String> {
        if idx >= self.storyboard_cards.len() {
            return Err("Invalid storyboard card".to_string());
        }
        let card = self.storyboard_cards[idx].clone();
        if !self.comfy.is_port_open() {
            self.log_comfy_embed(format!(
                "Storyboard '{}' -> ComfyUI: server not reachable at {}",
                card.title,
                self.comfy.url()
            ));
            return Err("ComfyUI server not reachable. Start ComfyUI and try again.".to_string());
        }
        let workflow_id = card
            .workflow_id
            .ok_or_else(|| "Select a workflow before queuing.".to_string())?;
        let preset = self
            .storyboard_workflows
            .iter()
            .find(|p| p.id == workflow_id)
            .cloned()
            .ok_or_else(|| "Selected workflow is no longer available.".to_string())?;
        let workflow_json = std::fs::read_to_string(&preset.path)
            .map_err(|e| format!("Failed to read workflow: {}", e))?;
        let preset_output = self
            .storyboard_workflows
            .iter()
            .find(|p| p.id == workflow_id)
            .map(|p| p.output_kind)
            .unwrap_or_default();
        let reference_path = std::path::Path::new(card.reference_path.trim());
        let reference_required = matches!(
            preset_output,
            StoryboardWorkflowOutputKind::Video | StoryboardWorkflowOutputKind::ImageAndVideo
        );
        let image_name = if reference_path.as_os_str().is_empty() {
            if reference_required {
                return Err("Reference path is empty.".to_string());
            }
            String::new()
        } else {
            if !reference_path.exists() {
                if reference_required {
                    return Err("Reference path does not exist.".to_string());
                }
            }
            if reference_path.exists() {
                self.copy_storyboard_reference_to_comfy(reference_path, &card)?
            } else {
                String::new()
            }
        };
        let mut resolved_file_inputs: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for (map_key, value) in card.workflow_inputs.iter() {
            if let StoryboardInputValue::File(path) = value {
                let trimmed = path.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let source_path = std::path::Path::new(trimmed);
                if source_path.exists() && source_path.is_file() {
                    let rel = self.copy_storyboard_input_to_comfy(source_path, &card, map_key)?;
                    resolved_file_inputs.insert(map_key.clone(), rel);
                } else {
                    resolved_file_inputs.insert(map_key.clone(), trimmed.to_string());
                }
            }
        }
        let forced_prefix = self.storyboard_forced_filename_prefix(card.id);
        let video_settings_for_job = card.video_settings.as_ref().map(|settings| {
            let mut clone = settings.clone();
            clone.filename_prefix = forced_prefix.clone();
            clone
        });

        let (mut body, client_id) = self.build_storyboard_prompt(
            &workflow_json,
            &card,
            &image_name,
            &resolved_file_inputs,
            &card.workflow_inputs,
            &forced_prefix,
        )?;
        let url = format!("{}/prompt", self.comfy.url().trim_end_matches('/'));

        if let serde_json::Value::Object(ref mut body_obj) = body {
            let entry = body_obj
                .entry("extra_data".to_string())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            if let serde_json::Value::Object(ref mut extra_map) = entry {
                let api_key = self.comfy_api_key.trim();
                if api_key.is_empty() {
                    extra_map.remove("api_key_comfy_org");
                } else {
                    extra_map.insert(
                        "api_key_comfy_org".to_string(),
                        serde_json::Value::String(api_key.to_string()),
                    );
                }
            }
        }

        let sampler_names: Vec<String> = [
            "euler",
            "euler_ancestral",
            "heun",
            "dpmpp_2m",
            "dpmpp_2m_sde",
            "dpmpp_3m_sde",
            "k_lms",
            "ddim",
        ]
        .into_iter()
        .map(|s| s.to_string())
        .collect();

        if let Some(prompt_value) = body.get_mut("prompt") {
            normalize_prompt_in_place(prompt_value, &sampler_names)
                .map_err(|err| format!("Failed to normalize prompt: {}", err))?;
        } else {
            return Err("Prompt payload missing 'prompt' section.".to_string());
        }

        if let Ok(pretty) = serde_json::to_string_pretty(&body) {
            let _ = std::fs::write("/tmp/last_comfy_payload.json", pretty);
        }

        let payload = serde_json::to_string(&body)
            .map_err(|err| format!("Failed to serialize prompt: {}", err))?;
        let auth_header = self.comfy_authorization_header();
        let mut request = self.comfy_http_agent.post(&url);
        request = request.set("Content-Type", "application/json");
        if let Some(ref auth) = auth_header {
            request = request.set("Authorization", auth);
        }
        let result = request.send_string(&payload);
        match result {
            Ok(resp) => {
                let body_text = resp.into_string().unwrap_or_default();
                let mut prompt_id: Option<String> = None;
                if !body_text.trim().is_empty() {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&body_text) {
                        if let Some(pid) = value.get("prompt_id").and_then(|v| v.as_str()) {
                            prompt_id = Some(pid.to_string());
                        }
                    }
                }
                if let Some(existing) = self.comfy_storyboard_jobs.get(&card.id) {
                    if let Some(prev_pid) = existing.prompt_id.as_ref() {
                        self.comfy_prompt_to_card.remove(prev_pid);
                    }
                }
                let mut inputs_map = serde_json::Map::new();
                for (key, value) in &card.workflow_inputs {
                    inputs_map.insert(key.clone(), value.to_json());
                }
                let workflow_inputs_json = serde_json::Value::Object(inputs_map);
                let fps_value = {
                    let num = self.seq.fps.num.max(1) as f32;
                    let den = self.seq.fps.den.max(1) as f32;
                    (num / den).max(1.0)
                };
                let queued_at = chrono::Utc::now();
                let job_record = ComfyStoryboardJob {
                    prompt_id: prompt_id.clone(),
                    prefix: forced_prefix.clone(),
                    workflow_name: Some(preset.name.clone()),
                    card_title: card.title.clone(),
                    card_description: card.description.clone(),
                    reference_path: card.reference_path.clone(),
                    duration_seconds: card.duration_seconds,
                    fps: fps_value,
                    video_settings: video_settings_for_job.clone(),
                    workflow_inputs: workflow_inputs_json,
                    last_output: None,
                    queued_at,
                };
                self.comfy_storyboard_jobs.insert(card.id, job_record);
                if let Err(err) = self.persist_storyboard_to_settings() {
                    tracing::warn!(
                        "Failed to persist storyboard after queueing Comfy job: {}",
                        err
                    );
                }
                if let Some(ref pid) = prompt_id {
                    self.comfy_known_prompts.insert(pid.clone());
                    self.comfy_jobs.insert(
                        pid.clone(),
                        ComfyJobInfo {
                            status: ComfyJobStatus::Queued,
                            progress: 0.0,
                            updated_at: Instant::now(),
                        },
                    );
                    self.comfy_prompt_to_card.insert(pid.clone(), card.id);
                }
                let queued_timestamp = chrono::Local::now().format("%H:%M:%S");
                self.set_storyboard_status(
                    card.id,
                    format!("Queued in ComfyUI ({})", queued_timestamp),
                );
                self.push_comfy_alert(
                    format!("Queued '{}' in ComfyUI", card.title),
                    ComfyAlertKind::Info,
                    std::time::Duration::from_secs(6),
                );
                // Ensure the websocket monitor is active so we receive status/progress.
                self.comfy_ws_monitor = true;
                self.comfy_last_queue_poll = None;

                let prompt_fragment = prompt_id
                    .as_ref()
                    .map(|pid| pid.chars().take(8).collect::<String>())
                    .unwrap_or_else(|| "unknown".to_string());
                let client_fragment = client_id.chars().take(8).collect::<String>();
                let summary = format!(
                    "Queued '{}' with workflow '{}' (prompt {}, client {})",
                    card.title, preset.name, prompt_fragment, client_fragment
                );
                self.comfy_import_logs.push_back(summary.clone());
                if self.comfy_import_logs.len() > 256 {
                    self.comfy_import_logs.pop_front();
                }
                self.log_comfy_embed(format!(
                    "Storyboard '{}' queued in ComfyUI (prompt {}, client {})",
                    card.title, prompt_fragment, client_fragment
                ));
                Ok(())
            }
            Err(ureq::Error::Status(code, resp)) => {
                let text = resp.into_string().unwrap_or_default();
                self.log_comfy_embed(format!(
                    "Storyboard '{}' failed to queue: ComfyUI {} {}",
                    card.title, code, text
                ));
                if matches!(code, 401 | 403) {
                    let message = if self.comfy_api_key.trim().is_empty() {
                        "Comfy.org API key required. Enter it in ComfyUI settings."
                    } else {
                        "Comfy.org rejected the stored API key. Re-enter it in ComfyUI settings."
                    };
                    self.push_comfy_alert(
                        message,
                        ComfyAlertKind::Warning,
                        std::time::Duration::from_secs(6),
                    );
                }
                Err(format!("ComfyUI {code}: {}", text))
            }
            Err(err) => {
                self.log_comfy_embed(format!(
                    "Storyboard '{}' failed to queue: {}",
                    card.title, err
                ));
                Err(format!("Failed to reach ComfyUI: {}", err))
            }
        }
    }

    fn storyboard_queue_all_cards(&mut self) {
        if self.storyboard_batch_busy {
            return;
        }
        if self.storyboard_cards.is_empty() {
            return;
        }
        if !self.comfy.is_port_open() {
            self.push_comfy_alert(
                "ComfyUI server not reachable. Start ComfyUI and try again.".to_string(),
                ComfyAlertKind::Warning,
                Duration::from_secs(6),
            );
            self.log_comfy_embed("Storyboard batch queue aborted: ComfyUI offline");
            return;
        }
        self.storyboard_batch_busy = true;
        let total = self.storyboard_cards.len();
        let mut queued = 0usize;
        let mut failures: Vec<(String, String)> = Vec::new();
        for idx in 0..total {
            let result = self.storyboard_send_to_comfy(idx);
            if let Some(card) = self.storyboard_cards.get_mut(idx) {
                match result {
                    Ok(_) => {
                        card.workflow_error = None;
                        queued += 1;
                    }
                    Err(err) => {
                        card.workflow_error = Some(err.clone());
                        failures.push((card.title.clone(), err));
                    }
                }
            } else if let Err(err) = result {
                failures.push((format!("Card {}", idx + 1), err));
            }
        }
        self.storyboard_batch_busy = false;
        self.log_comfy_embed(format!(
            "Storyboard batch queue finished: {} queued, {} skipped",
            queued,
            failures.len()
        ));
        if queued > 0 {
            self.push_comfy_alert(
                format!(
                    "Queued {} storyboard job{} in ComfyUI",
                    queued,
                    if queued == 1 { "" } else { "s" }
                ),
                ComfyAlertKind::Info,
                Duration::from_secs(6),
            );
        }
        if !failures.is_empty() {
            let preview: Vec<String> = failures
                .iter()
                .take(3)
                .map(|(title, _)| title.clone())
                .collect();
            let summary = if preview.is_empty() {
                String::new()
            } else if preview.len() == failures.len() {
                preview.join(", ")
            } else {
                format!("{}…", preview.join(", "))
            };
            let mut message = format!("Skipped {} card(s)", failures.len());
            if !summary.is_empty() {
                message.push_str(": ");
                message.push_str(&summary);
            }
            self.push_comfy_alert(message, ComfyAlertKind::Warning, Duration::from_secs(7));
            for (title, err) in failures.into_iter().take(8) {
                self.log_comfy_embed(format!("Storyboard batch skipped '{}': {}", title, err));
            }
        }
    }

    fn handle_comfy_ws_event(&mut self, event: ComfyWsEvent) {
        match event {
            ComfyWsEvent::Queue { pending, running } => {
                let now = Instant::now();
                self.comfy_queue_pending = pending.len();
                self.comfy_queue_running = running.len();
                self.comfy_last_queue_poll = Some(now);

                let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
                for pid in pending.iter() {
                    let entry = self.comfy_jobs.entry(pid.clone()).or_insert(ComfyJobInfo {
                        status: ComfyJobStatus::Queued,
                        progress: 0.0,
                        updated_at: now,
                    });
                    entry.status = ComfyJobStatus::Queued;
                    entry.updated_at = now;
                    seen.insert(pid.clone());
                }
                for pid in running.iter() {
                    let entry = self.comfy_jobs.entry(pid.clone()).or_insert(ComfyJobInfo {
                        status: ComfyJobStatus::Running,
                        progress: 0.0,
                        updated_at: now,
                    });
                    entry.status = ComfyJobStatus::Running;
                    entry.updated_at = now;
                    if entry.progress < 0.01 {
                        entry.progress = 0.01;
                    }
                    seen.insert(pid.clone());
                }
                for (pid, info) in self.comfy_jobs.iter_mut() {
                    if !seen.contains(pid) {
                        if matches!(
                            info.status,
                            ComfyJobStatus::Queued | ComfyJobStatus::Running
                        ) {
                            info.status = ComfyJobStatus::Completed;
                            info.progress = 1.0;
                            info.updated_at = now;
                        }
                    }
                }
            }
            ComfyWsEvent::Progress {
                prompt_id,
                value,
                max,
            } => {
                let now = Instant::now();
                let entry = self
                    .comfy_jobs
                    .entry(prompt_id.clone())
                    .or_insert(ComfyJobInfo {
                        status: ComfyJobStatus::Running,
                        progress: 0.0,
                        updated_at: now,
                    });
                entry.status = ComfyJobStatus::Running;
                let progress = if max <= f32::EPSILON {
                    0.0
                } else {
                    (value / max).clamp(0.0, 1.0)
                };
                entry.progress = progress;
                entry.updated_at = now;
                self.comfy_known_prompts.insert(prompt_id.clone());
                if progress.is_finite() && progress > 0.0 {
                    let pct = (progress * 100.0).clamp(0.0, 100.0);
                    let stamp = chrono::Local::now().format("%H:%M:%S");
                    self.set_storyboard_status_for_prompt(
                        &prompt_id,
                        format!("Rendering in ComfyUI… {:>3.0}% ({})", pct, stamp),
                    );
                }
            }
            ComfyWsEvent::ExecutionStart { prompt_id } => {
                let now = Instant::now();
                let entry = self
                    .comfy_jobs
                    .entry(prompt_id.clone())
                    .or_insert(ComfyJobInfo {
                        status: ComfyJobStatus::Running,
                        progress: 0.0,
                        updated_at: now,
                    });
                entry.status = ComfyJobStatus::Running;
                entry.updated_at = now;
                self.comfy_known_prompts.insert(prompt_id.clone());
                let stamp = chrono::Local::now().format("%H:%M:%S");
                self.set_storyboard_status_for_prompt(
                    &prompt_id,
                    format!("Rendering in ComfyUI… ({})", stamp),
                );
            }
            ComfyWsEvent::ExecutionEnd { prompt_id } => {
                let now = Instant::now();
                let entry = self
                    .comfy_jobs
                    .entry(prompt_id.clone())
                    .or_insert(ComfyJobInfo {
                        status: ComfyJobStatus::Completed,
                        progress: 1.0,
                        updated_at: now,
                    });
                entry.status = ComfyJobStatus::Completed;
                entry.progress = 1.0;
                entry.updated_at = now;
                self.comfy_known_prompts.insert(prompt_id.clone());
                let stamp = chrono::Local::now().format("%H:%M:%S");
                self.set_storyboard_status_for_prompt(
                    &prompt_id,
                    format!("ComfyUI render complete ({}) – importing result…", stamp),
                );
                if let Some(card_id) = self.comfy_prompt_to_card.get(&prompt_id).cloned() {
                    if let Some(card) = self.storyboard_cards.iter().find(|c| c.id == card_id) {
                        self.push_comfy_alert(
                            format!("ComfyUI job completed for '{}'", card.title),
                            ComfyAlertKind::Success,
                            std::time::Duration::from_secs(6),
                        );
                    } else {
                        self.push_comfy_alert(
                            "ComfyUI job completed".to_string(),
                            ComfyAlertKind::Success,
                            std::time::Duration::from_secs(6),
                        );
                    }
                }
                if let Some(card_id) = self.comfy_prompt_to_card.remove(&prompt_id) {
                    if let Some(job) = self.comfy_storyboard_jobs.get_mut(&card_id) {
                        job.prompt_id = None;
                    }
                }
            }
        }
    }

    fn prune_comfy_jobs(&mut self) {
        let now = Instant::now();
        self.comfy_jobs.retain(|_, info| {
            if matches!(
                info.status,
                ComfyJobStatus::Completed | ComfyJobStatus::Failed
            ) {
                now.duration_since(info.updated_at) < Duration::from_secs(30)
            } else {
                true
            }
        });
    }

    fn refresh_comfy_queue_http(&mut self) {
        let base = self.comfy.url();
        if base.trim().is_empty() {
            return;
        }
        let url = format!("{}/queue", base.trim_end_matches('/'));
        let auth_header = self.comfy_authorization_header();
        let mut request = self.comfy_http_agent.get(&url);
        if let Some(ref auth) = auth_header {
            request = request.set("Authorization", auth);
        }
        match request.call() {
            Ok(resp) => {
                if let Ok(body) = resp.into_string() {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&body) {
                        let pending_ids = value
                            .get("queue_pending")
                            .and_then(|arr| arr.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|item| {
                                        if let Some(array) = item.as_array() {
                                            array
                                                .get(1)
                                                .and_then(|v| v.as_str())
                                                .map(|s| s.to_string())
                                        } else {
                                            None
                                        }
                                    })
                                    .collect::<Vec<String>>()
                            })
                            .unwrap_or_default();
                        let running_ids = value
                            .get("queue_running")
                            .and_then(|arr| arr.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|item| {
                                        if let Some(array) = item.as_array() {
                                            array
                                                .get(1)
                                                .and_then(|v| v.as_str())
                                                .map(|s| s.to_string())
                                        } else {
                                            None
                                        }
                                    })
                                    .collect::<Vec<String>>()
                            })
                            .unwrap_or_default();
                        self.handle_comfy_ws_event(ComfyWsEvent::Queue {
                            pending: pending_ids,
                            running: running_ids,
                        });
                    }
                }
            }
            Err(_) => {}
        }
    }

    fn process_comfy_events(&mut self) {
        while let Ok(event) = self.comfy_ws_rx.try_recv() {
            self.handle_comfy_ws_event(event);
        }
        let should_poll = (!self.comfy_jobs.is_empty()
            || self.comfy_queue_pending + self.comfy_queue_running > 0)
            && self
                .comfy_last_queue_poll
                .map(|last| last.elapsed() > Duration::from_secs(3))
                .unwrap_or(true);
        if should_poll {
            self.refresh_comfy_queue_http();
            self.comfy_last_queue_poll = Some(Instant::now());
        }
        self.prune_comfy_jobs();
    }

    fn push_comfy_alert(
        &mut self,
        message: impl Into<String>,
        kind: ComfyAlertKind,
        duration: std::time::Duration,
    ) {
        let alert = ComfyAlert {
            message: message.into(),
            kind,
            expires_at: Instant::now() + duration,
        };
        self.comfy_alerts.push_back(alert);
        if self.comfy_alerts.len() > 8 {
            self.comfy_alerts.pop_front();
        }
    }

    fn prune_comfy_alerts(&mut self) {
        let now = Instant::now();
        while let Some(alert) = self.comfy_alerts.front() {
            if alert.expires_at <= now {
                self.comfy_alerts.pop_front();
            } else {
                break;
            }
        }
    }

    fn load_proxy_settings(&mut self) {
        match self.db.get_project_settings_json(&self.project_id) {
            Ok(settings) => {
                if let Some(value) = settings.get("proxy") {
                    if let Ok(stored) =
                        serde_json::from_value::<PersistedProxySettings>(value.clone())
                    {
                        self.proxy_mode_user = stored.proxy_mode;
                        self.auto_proxy_setting = stored.auto_proxy;
                    }
                }
            }
            Err(err) => {
                eprintln!("Failed to load proxy settings: {err}");
            }
        }
        self.proxy_mode_override = None;
    }

    fn load_comfy_settings(&mut self) {
        match self.db.get_project_settings_json(&self.project_id) {
            Ok(settings) => {
                if let Some(value) = settings.get("comfy") {
                    if let Ok(stored) =
                        serde_json::from_value::<PersistedComfySettings>(value.clone())
                    {
                        self.comfy_api_key = stored.api_key.clone();
                        return;
                    }
                }
                self.comfy_api_key.clear();
            }
            Err(err) => {
                eprintln!("Failed to load ComfyUI settings: {err}");
                self.comfy_api_key.clear();
            }
        }
    }

    fn persist_proxy_settings(&mut self) -> anyhow::Result<()> {
        let mut settings = self.db.get_project_settings_json(&self.project_id)?;
        let mut map = match settings {
            serde_json::Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        let stored = PersistedProxySettings {
            proxy_mode: self.proxy_mode_user,
            auto_proxy: self.auto_proxy_setting,
        };
        map.insert("proxy".to_string(), serde_json::to_value(stored)?);
        let value = serde_json::Value::Object(map);
        self.db
            .update_project_settings_json(&self.project_id, &value)?;
        Ok(())
    }

    #[allow(dead_code)]
    fn persist_comfy_settings(&mut self) -> anyhow::Result<()> {
        let mut settings = self.db.get_project_settings_json(&self.project_id)?;
        let mut map = match settings {
            serde_json::Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        let stored = PersistedComfySettings {
            api_key: self.comfy_api_key.clone(),
        };
        map.insert("comfy".to_string(), serde_json::to_value(stored)?);
        let value = serde_json::Value::Object(map);
        self.db
            .update_project_settings_json(&self.project_id, &value)?;
        Ok(())
    }

    fn effective_proxy_mode(&self) -> ProxyMode {
        self.proxy_mode_override.unwrap_or(self.proxy_mode_user)
    }

    fn asset_for_path(&mut self, path: &str) -> Option<project::AssetRow> {
        let key = path.to_string();
        if let Some(entry) = self.asset_cache.get(&key) {
            if entry.last_refresh.elapsed() < Duration::from_secs(3) {
                return Some(entry.asset.clone());
            }
        }
        match self.db.find_asset_by_path(&self.project_id, path) {
            Ok(Some(asset)) => {
                self.asset_cache.insert(
                    key,
                    CachedAssetEntry {
                        asset: asset.clone(),
                        last_refresh: Instant::now(),
                    },
                );
                Some(asset)
            }
            Ok(None) => None,
            Err(err) => {
                eprintln!("Failed to fetch asset for path {path}: {err}");
                None
            }
        }
    }

    fn refresh_asset_cache_entry(&mut self, asset_id: &str) {
        if let Ok(asset) = self.db.get_asset(asset_id) {
            self.asset_cache.insert(
                asset.src_abs.clone(),
                CachedAssetEntry {
                    asset,
                    last_refresh: Instant::now(),
                },
            );
        }
    }

    fn determine_playback_path(&mut self, original_path: &str) -> PlaybackPathDecision {
        let asset = self.asset_for_path(original_path);
        let mut mode = self.effective_proxy_mode();
        let mut queue_reason = None;
        let mut using_proxy = false;
        let mut using_optimized = false;

        let decode_path = if let Some(asset_row) = asset.as_ref() {
            if self.is_proxy_preview_forced(&asset_row.id) {
                if asset_row.is_proxy_ready {
                    mode = ProxyMode::ProxyPreferred;
                } else {
                    self.proxy_preview_overrides.remove(&asset_row.id);
                }
            }

            let optimized_candidate = self.cache_manager.cached_output_path(
                std::path::Path::new(&asset_row.src_abs),
                PreferredCodec::ProRes422,
            );

            match PlaybackSelector::select_path(asset_row, mode, optimized_candidate.as_deref()) {
                Some(selection) => {
                    match selection.source {
                        PlaybackSource::Optimized => using_optimized = true,
                        PlaybackSource::Proxy => using_proxy = true,
                        PlaybackSource::Original => {}
                    }
                    if matches!(mode, ProxyMode::ProxyPreferred)
                        && !asset_row.is_proxy_ready
                        && !using_proxy
                        && !using_optimized
                    {
                        queue_reason = Some(ProxyReason::Mode);
                    }
                    selection.path
                }
                None => {
                    queue_reason = Some(ProxyReason::Mode);
                    asset_row.src_abs.clone()
                }
            }
        } else {
            original_path.to_string()
        };

        PlaybackPathDecision {
            decode_path,
            asset,
            using_proxy,
            using_optimized,
            queue_reason,
        }
    }

    fn prime_asset_for_timeline(&mut self, asset: &project::AssetRow) {
        if !asset.kind.eq_ignore_ascii_case("video") {
            self.queue_analysis_jobs_for_asset(asset);
            return;
        }
        if self.pending_heavy_asset_set.insert(asset.id.clone()) {
            self.pending_heavy_assets.push_back(asset.id.clone());
            tracing::info!(
                asset = %asset.id,
                "scheduled deferred media preparation"
            );
        }
    }

    fn dispatch_heavy_tasks_for_asset(&mut self, asset: &project::AssetRow) {
        self.queue_analysis_jobs_for_asset(asset);
        self.queue_optimized_media_for_asset(asset);
        self.consider_proxy_for_asset(asset, ProxyReason::Timeline);
    }

    fn queue_optimized_media_for_asset(&self, asset: &project::AssetRow) {
        if !asset.kind.eq_ignore_ascii_case("video") {
            return;
        }
        let codec_hint = asset.codec.clone();
        let spec = CacheJobSpec {
            source_path: std::path::PathBuf::from(&asset.src_abs),
            force_container_mov: true,
            preferred_codec: PreferredCodec::ProRes422,
            source_codec: codec_hint,
        };
        let job_id = self.cache_manager.submit_cache_job(spec);
        tracing::info!(
            target = "cache",
            asset = %asset.id,
            path = %asset.src_abs,
            job = job_id.0,
            "queued optimized media job"
        );
    }

    fn queue_analysis_jobs_for_asset(&mut self, asset: &project::AssetRow) {
        if self.auto_analysis_requests.contains(&asset.id) {
            return;
        }
        let Some(jobs) = &self.jobs else {
            return;
        };
        use jobs_crate::{JobKind, JobSpec};

        let kinds = [JobKind::Waveform, JobKind::Thumbnails, JobKind::SeekIndex];
        let mut queued_any = false;
        for kind in kinds {
            let job_id = jobs.enqueue(JobSpec {
                asset_id: asset.id.clone(),
                kind,
                priority: 0,
            });
            queued_any = true;
            if let Err(err) = self.db.enqueue_job(
                &job_id,
                &asset.id,
                match kind {
                    JobKind::Waveform => "waveform",
                    JobKind::Thumbnails => "thumbs",
                    JobKind::SeekIndex => "seek",
                    _ => "analysis",
                },
                0,
            ) {
                tracing::debug!(
                    asset = %asset.id,
                    job = %job_id,
                    kind = ?kind,
                    "failed to record job in db: {err}"
                );
            }
        }
        if queued_any {
            self.auto_analysis_requests.insert(asset.id.clone());
        }
    }

    fn process_pending_heavy_assets(&mut self) {
        if self.playback_clock.playing {
            return;
        }
        if let Some(last) = self.last_heavy_job_dispatch {
            if last.elapsed() < std::time::Duration::from_secs(1) {
                return;
            }
        }
        if let Some(next_id) = self.pending_heavy_assets.front().cloned() {
            match self.db.get_asset(&next_id) {
                Ok(asset) => {
                    tracing::info!(
                        asset = %asset.id,
                        "starting deferred media preparation"
                    );
                    self.dispatch_heavy_tasks_for_asset(&asset);
                }
                Err(err) => {
                    tracing::warn!(
                        asset = %next_id,
                        error = %err,
                        "failed to load asset for deferred media preparation"
                    );
                }
            }
            self.pending_heavy_assets.pop_front();
            self.pending_heavy_asset_set.remove(&next_id);
            self.last_heavy_job_dispatch = Some(Instant::now());
        }
    }

    fn queue_proxy_for_asset(
        &mut self,
        asset: &project::AssetRow,
        reason: ProxyReason,
        force: bool,
    ) {
        if asset.is_proxy_ready && !force {
            self.append_proxy_log(
                &asset.id,
                format!(
                    "Proxy already ready; using existing file {}",
                    asset
                        .proxy_path
                        .as_deref()
                        .unwrap_or("<unknown proxy path>")
                ),
            );
            return;
        }
        if self.auto_proxy_requests.contains(&asset.id) {
            return;
        }
        if let Some(status) = self.proxy_status.get(&asset.id) {
            if matches!(status, ProxyStatus::Pending | ProxyStatus::Running { .. }) {
                return;
            }
        }
        if let Some(queue) = &self.proxy_queue {
            queue.enqueue(crate::proxy_queue::ProxyEnqueueRequest {
                project_id: self.project_id.clone(),
                asset_id: asset.id.clone(),
                reason,
                force,
            });
            self.auto_proxy_requests.insert(asset.id.clone());
            let name = std::path::Path::new(&asset.src_abs)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| asset.id.clone());
            self.push_comfy_alert(
                format!("Generating proxy for {name}…"),
                ComfyAlertKind::Info,
                Duration::from_secs(4),
            );
            self.append_proxy_log(
                &asset.id,
                format!(
                    "Queued proxy job (reason: {}, force: {})",
                    reason.as_str(),
                    force
                ),
            );
        }
    }

    fn is_proxy_preview_forced(&self, asset_id: &str) -> bool {
        self.proxy_preview_overrides.contains(asset_id)
    }

    fn force_proxy_preview_for_asset(&mut self, asset: &project::AssetRow) {
        if self.proxy_preview_overrides.insert(asset.id.clone()) {
            let name = std::path::Path::new(&asset.src_abs)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| asset.id.clone());
            self.append_proxy_log(&asset.id, "Preview switched to proxy media");
            self.push_comfy_alert(
                format!("Previewing proxy for {}", name),
                ComfyAlertKind::Info,
                Duration::from_secs(4),
            );
        }
    }

    fn restore_original_preview_for_asset(&mut self, asset_id: &str) {
        if self.proxy_preview_overrides.remove(asset_id) {
            self.append_proxy_log(asset_id, "Preview reverted to original media");
            self.push_comfy_alert(
                "Previewing original media",
                ComfyAlertKind::Info,
                Duration::from_secs(4),
            );
        }
    }

    fn delete_proxy_for_asset(&mut self, asset: &project::AssetRow) {
        let Some(proxy_path) = asset.proxy_path.as_ref() else {
            return;
        };
        let path_buf = std::path::PathBuf::from(proxy_path);
        let remove_result = std::fs::remove_file(&path_buf);
        let mut details = project::AssetMediaDetails::default();
        details.proxy_path = None;
        details.is_proxy_ready = Some(false);
        if let Err(err) = self.db.update_asset_media_details(&asset.id, &details) {
            tracing::warn!(
                asset = %asset.id,
                "failed to update asset after proxy delete: {err}"
            );
        }
        if remove_result.is_err() {
            tracing::warn!(
                path = %path_buf.display(),
                "failed to remove proxy file"
            );
        }
        self.proxy_status.remove(&asset.id);
        self.proxy_preview_overrides.remove(&asset.id);
        self.append_proxy_log(&asset.id, "Proxy deleted by user");
        self.push_comfy_alert(
            "Proxy file removed",
            ComfyAlertKind::Info,
            std::time::Duration::from_secs(4),
        );
    }

    fn append_proxy_log(&mut self, asset_id: &str, message: impl Into<String>) {
        use std::collections::VecDeque;
        let entry = format!(
            "[{}] {}",
            chrono::Local::now().format("%H:%M:%S"),
            message.into()
        );
        let logs = self
            .proxy_logs
            .entry(asset_id.to_string())
            .or_insert_with(|| VecDeque::with_capacity(32));
        logs.push_back(entry);
        if logs.len() > 64 {
            logs.pop_front();
        }
    }

    fn consider_proxy_for_asset(&mut self, asset: &project::AssetRow, reason: ProxyReason) {
        if !asset.kind.eq_ignore_ascii_case("video") || asset.is_proxy_ready {
            return;
        }
        let media_info = Self::asset_to_media_info(asset);
        let needs_proxy = self.auto_proxy_setting.should_queue_proxy(
            true,
            Some(&media_info),
            self.hardware_caps.as_ref(),
        );
        if needs_proxy {
            self.queue_proxy_for_asset(asset, reason, false);
        }
    }

    fn asset_to_media_info(asset: &project::AssetRow) -> MediaInfoData {
        let fps = match (asset.fps_num, asset.fps_den) {
            (Some(n), Some(d)) if n > 0 && d > 0 => Some(n as f64 / d as f64),
            _ => None,
        };
        let codec_upper = asset
            .codec
            .as_ref()
            .map(|c| c.to_ascii_uppercase())
            .unwrap_or_default();
        let is_intra = codec_upper.contains("PRORES") || codec_upper.contains("DNX");
        MediaInfoData {
            path: std::path::PathBuf::from(&asset.src_abs),
            kind: match asset.kind.to_lowercase().as_str() {
                "image" => MediaKind::Image,
                "audio" => MediaKind::Audio,
                _ => MediaKind::Video,
            },
            width: asset.width.filter(|v| *v > 0).map(|v| v as u32),
            height: asset.height.filter(|v| *v > 0).map(|v| v as u32),
            duration_seconds: asset.duration_seconds,
            fps_num: asset.fps_num.filter(|v| *v > 0).map(|v| v as u32),
            fps_den: asset.fps_den.filter(|v| *v > 0).map(|v| v as u32),
            fps,
            is_variable_framerate: asset.is_variable_framerate,
            codec: asset.codec.clone(),
            codec_profile: None,
            bitrate_mbps: asset.bitrate_mbps,
            bit_depth: asset.bit_depth.filter(|v| *v > 0).map(|v| v as u32),
            is_hdr: asset.is_hdr,
            is_inter_frame: !is_intra,
            audio_channels: asset.audio_channels.filter(|v| *v > 0).map(|v| v as u32),
            sample_rate: asset.sample_rate.filter(|v| *v > 0).map(|v| v as u32),
            has_alpha: false,
            has_multiple_video_streams: false,
            file_size_bytes: None,
        }
    }

    fn update_playback_adaptive(
        &mut self,
        lagging: bool,
        frame_dur: f64,
        asset: Option<&project::AssetRow>,
        using_proxy: bool,
    ) {
        if !matches!(self.engine.state, PlayState::Playing) {
            self.playback_lag_frames = 0;
            self.playback_stable_frames = 0;
            return;
        }
        if frame_dur <= 0.0 {
            return;
        }
        if lagging {
            self.playback_lag_frames = self.playback_lag_frames.saturating_add(1);
            self.playback_stable_frames = 0;
            if self.playback_lag_frames >= 20 {
                if let Some(asset) = asset {
                    if !using_proxy {
                        if asset.is_proxy_ready {
                            if !self.is_proxy_preview_forced(&asset.id) {
                                self.force_proxy_preview_for_asset(asset);
                            }
                        } else {
                            if self.proxy_mode_override != Some(ProxyMode::ProxyPreferred) {
                                self.proxy_mode_override = Some(ProxyMode::ProxyPreferred);
                                self.push_comfy_alert(
                                    "Proxy Preferred enabled after sustained frame drops",
                                    ComfyAlertKind::Info,
                                    Duration::from_secs(4),
                                );
                            }
                            self.queue_proxy_for_asset(asset, ProxyReason::PlaybackLag, true);
                        }
                    }
                }
                self.playback_lag_frames = 0;
            }
        } else {
            self.playback_stable_frames = self.playback_stable_frames.saturating_add(1);
            if self.playback_lag_frames > 0 {
                self.playback_lag_frames -= 1;
            }
            if self.playback_stable_frames >= 240 {
                self.playback_stable_frames = 0;
            }
        }
    }

    fn set_storyboard_status(&mut self, card_id: uuid::Uuid, status: impl Into<String>) {
        if let Some(card) = self
            .storyboard_cards
            .iter_mut()
            .find(|card| card.id == card_id)
        {
            card.workflow_status = Some(status.into());
        }
    }

    fn set_storyboard_status_for_prompt(&mut self, prompt_id: &str, status: impl Into<String>) {
        if let Some(card_id) = self.comfy_prompt_to_card.get(prompt_id).cloned() {
            self.set_storyboard_status(card_id, status);
        }
    }

    fn apply_comfy_output_to_storyboard(
        &mut self,
        ctx: &egui::Context,
        dest: &std::path::Path,
    ) -> Option<(uuid::Uuid, serde_json::Value)> {
        let Some(file_name_os) = dest.file_name() else {
            return None;
        };
        let Some(file_name) = file_name_os.to_str() else {
            return None;
        };
        let file_name_lower = file_name.to_ascii_lowercase();
        let mut matched_card: Option<uuid::Uuid> = None;

        for (card_id, job) in self.comfy_storyboard_jobs.iter() {
            if let Some(prev) = job.last_output.as_ref() {
                if prev == dest {
                    return None;
                }
            }
            let mut matched = false;
            if !job.prefix.trim().is_empty() {
                let prefix_lower = job.prefix.trim().to_ascii_lowercase();
                if file_name_lower.starts_with(&prefix_lower) {
                    matched = true;
                } else if let Some(prefix_name) = std::path::Path::new(&prefix_lower)
                    .file_name()
                    .and_then(|s| s.to_str())
                {
                    if !prefix_name.is_empty()
                        && file_name_lower.starts_with(&prefix_name.to_ascii_lowercase())
                    {
                        matched = true;
                    }
                }
            }
            if !matched {
                let short_id: String = card_id.to_string().chars().take(8).collect();
                if file_name_lower.contains(&short_id.to_ascii_lowercase()) {
                    matched = true;
                }
            }
            if matched {
                matched_card = Some(*card_id);
                break;
            }
        }

        let Some(card_id) = matched_card else {
            return None;
        };

        if let Some(idx) = self
            .storyboard_cards
            .iter()
            .position(|card| card.id == card_id)
        {
            let dest_str = dest.to_string_lossy().to_string();
            let mut should_reload = false;
            let mut log_title: Option<String> = None;
            let mut final_title: Option<String> = None;
            {
                let card = &mut self.storyboard_cards[idx];
                let import_stamp = chrono::Local::now().format("%H:%M:%S");
                card.workflow_status =
                    Some(format!("Importing result from ComfyUI… ({})", import_stamp));
                if card.reference_path != dest_str {
                    card.reference_path = dest_str.clone();
                    card.preview_error = None;
                    card.workflow_error = None;
                    log_title = Some(card.title.clone());
                    final_title = Some(card.title.clone());
                    should_reload = true;
                    if card.output_kind == StoryboardWorkflowOutputKind::Image
                        || card.output_kind == StoryboardWorkflowOutputKind::ImageAndVideo
                    {
                        if let Err(err) = self.persist_storyboard_to_settings() {
                            tracing::warn!(
                                "Failed to persist storyboard after Comfy import: {}",
                                err
                            );
                        }
                    }
                }
            }
            let metadata_value = if let Some(job) = self.comfy_storyboard_jobs.get_mut(&card_id) {
                if let Some(pid) = job.prompt_id.take() {
                    self.comfy_prompt_to_card.remove(&pid);
                }
                job.last_output = Some(dest.to_path_buf());
                let completed_at = chrono::Utc::now();
                let video_settings_json = job.video_settings.as_ref().map(|settings| {
                    serde_json::json!({
                        "filename_prefix": settings.filename_prefix,
                        "format": settings.format,
                        "codec": settings.codec,
                    })
                });
                Some(serde_json::json!({
                    "source": "comfy_storyboard",
                    "card_id": card_id.to_string(),
                    "card_title": job.card_title,
                    "card_description": job.card_description,
                    "reference_path": job.reference_path,
                    "duration_seconds": job.duration_seconds,
                    "fps": job.fps,
                    "workflow_name": job.workflow_name,
                    "workflow_inputs": job.workflow_inputs.clone(),
                    "video_settings": video_settings_json,
                    "queued_at": job.queued_at.to_rfc3339(),
                    "completed_at": completed_at.to_rfc3339(),
                    "output_path": dest.to_string_lossy(),
                }))
            } else {
                None
            };
            if should_reload {
                self.storyboard_load_preview(ctx, idx);
            }
            if let Some(title) = log_title {
                self.comfy_import_logs.push_back(format!(
                    "Storyboard '{}' reference updated to {}",
                    title, dest_str
                ));
                if self.comfy_import_logs.len() > 256 {
                    self.comfy_import_logs.pop_front();
                }
            }
            let complete_stamp = chrono::Local::now().format("%H:%M:%S");
            if let Some(card) = self.storyboard_cards.get_mut(idx) {
                card.workflow_status = Some(format!("Updated from ComfyUI ({})", complete_stamp));
            }
            if let Some(title) = final_title {
                self.push_comfy_alert(
                    format!("Storyboard '{}' updated from ComfyUI", title),
                    ComfyAlertKind::Success,
                    std::time::Duration::from_secs(6),
                );
            }
            if let Err(err) = self.persist_storyboard_to_settings() {
                tracing::warn!(
                    "Failed to persist storyboard after Comfy import metadata update: {}",
                    err
                );
            }
            return metadata_value.map(|meta| (card_id, meta));
        }
        None
    }

    fn copy_storyboard_reference_to_comfy(
        &self,
        source: &std::path::Path,
        card: &StoryboardCard,
    ) -> Result<String, String> {
        let repo = self
            .comfy
            .config()
            .repo_path
            .as_ref()
            .ok_or_else(|| "ComfyUI repo path not configured.".to_string())?;
        let dest_dir = repo.join("input").join("storyboard");
        std::fs::create_dir_all(&dest_dir)
            .map_err(|e| format!("Failed to prepare ComfyUI input folder: {}", e))?;
        let ext = source.extension().and_then(|e| e.to_str()).unwrap_or("png");
        let prefix = Self::sanitize_filename_component(&card.title);
        let short_id: String = card.id.to_string().chars().take(8).collect();
        let mut dest = dest_dir.join(format!("{}-{}.{}", prefix, short_id, ext));
        if dest.exists() {
            let mut i = 1;
            loop {
                let candidate = dest_dir.join(format!("{}-{}-{}.{}", prefix, short_id, i, ext));
                if !candidate.exists() {
                    dest = candidate;
                    break;
                }
                i += 1;
            }
        }
        std::fs::copy(source, &dest)
            .map_err(|e| format!("Failed to copy reference to ComfyUI: {}", e))?;
        let rel = format!(
            "storyboard/{}",
            dest.file_name()
                .and_then(|f| f.to_str())
                .unwrap_or("reference.png")
        );
        Ok(rel)
    }

    fn copy_storyboard_input_to_comfy(
        &self,
        source: &std::path::Path,
        card: &StoryboardCard,
        map_key: &str,
    ) -> Result<String, String> {
        let repo = self
            .comfy
            .config()
            .repo_path
            .as_ref()
            .ok_or_else(|| "ComfyUI repo path not configured.".to_string())?;
        let dest_dir = repo.join("input").join("storyboard");
        std::fs::create_dir_all(&dest_dir)
            .map_err(|e| format!("Failed to prepare ComfyUI input folder: {}", e))?;
        let ext = source.extension().and_then(|e| e.to_str()).unwrap_or("png");
        let prefix = Self::sanitize_filename_component(&card.title);
        let key_component = map_key
            .split(':')
            .last()
            .map(|part| Self::sanitize_filename_component(part))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "input".to_string());
        let short_id: String = card.id.to_string().chars().take(8).collect();
        let mut dest = dest_dir.join(format!("{}-{}-{}.{}", prefix, key_component, short_id, ext));
        if dest.exists() {
            let mut i = 1;
            loop {
                let candidate = dest_dir.join(format!(
                    "{}-{}-{}-{}.{}",
                    prefix, key_component, short_id, i, ext
                ));
                if !candidate.exists() {
                    dest = candidate;
                    break;
                }
                i += 1;
            }
        }
        std::fs::copy(source, &dest)
            .map_err(|e| format!("Failed to copy workflow asset to ComfyUI: {}", e))?;
        let rel = format!(
            "storyboard/{}",
            dest.file_name()
                .and_then(|f| f.to_str())
                .unwrap_or("input.png")
        );
        Ok(rel)
    }

    fn storyboard_duration_to_frames(&self, seconds: f32) -> i64 {
        let num = self.seq.fps.num.max(1) as f64;
        let den = self.seq.fps.den.max(1) as f64;
        let frames = (seconds.max(0.0) as f64) * (num / den);
        frames.round().max(1.0) as i64
    }

    fn storyboard_detect_asset_kind(
        &self,
        path: &str,
        video_settings: Option<&StoryboardVideoSettings>,
    ) -> StoryboardAssetKind {
        let ext = std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase());
        if let Some(ext) = ext.as_deref() {
            match ext {
                "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "exr" => {
                    return StoryboardAssetKind::Image;
                }
                "mp4" | "mov" | "webm" | "mkv" | "avi" | "m4v" | "mpg" | "mpeg" | "gifv" => {
                    return StoryboardAssetKind::Video;
                }
                _ => {}
            }
        }
        if let Some(settings) = video_settings {
            let format = settings.format.to_ascii_lowercase();
            let codec = settings.codec.to_ascii_lowercase();
            if format.contains("video")
                || format.contains("mp4")
                || format.contains("mov")
                || codec.contains("264")
                || codec.contains("265")
                || codec.contains("video")
            {
                return StoryboardAssetKind::Video;
            }
        }
        StoryboardAssetKind::Video
    }

    fn ensure_storyboard_track(
        &mut self,
        name: &str,
        kind: timeline_crate::TrackKind,
    ) -> Result<timeline_crate::TrackId, String> {
        if let Some(existing) = self
            .seq
            .graph
            .tracks
            .iter()
            .find(|t| t.name.eq_ignore_ascii_case(name))
        {
            return Ok(existing.id);
        }
        let track = timeline_crate::TrackBinding {
            id: timeline_crate::TrackId::new(),
            name: name.to_string(),
            kind,
            node_ids: Vec::new(),
        };
        let track_id = track.id;
        self::app_timeline::apply_timeline_command_impl(
            self,
            timeline_crate::TimelineCommand::UpsertTrack { track },
        )
        .map_err(|e| e.to_string())?;
        Ok(track_id)
    }

    fn ensure_storyboard_overlay_track(
        &mut self,
        overlay_index: &mut usize,
    ) -> Result<timeline_crate::TrackId, String> {
        let name = format!("Storyboard Overlay {}", *overlay_index);
        *overlay_index += 1;
        let overlay_track = timeline_crate::TrackBinding {
            id: timeline_crate::TrackId::new(),
            name,
            kind: timeline_crate::TrackKind::Video,
            node_ids: Vec::new(),
        };
        let overlay_id = overlay_track.id;
        self::app_timeline::apply_timeline_command_impl(
            self,
            timeline_crate::TimelineCommand::UpsertTrack {
                track: overlay_track,
            },
        )
        .map_err(|e| e.to_string())?;
        if let Some(audio_idx) = self
            .seq
            .graph
            .tracks
            .iter()
            .position(|t| matches!(t.kind, timeline_crate::TrackKind::Audio))
        {
            self::app_timeline::apply_timeline_command_impl(
                self,
                timeline_crate::TimelineCommand::MoveTrack {
                    track_id: overlay_id,
                    index: audio_idx,
                },
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(overlay_id)
    }

    fn transfer_storyboard_to_timeline(
        &mut self,
    ) -> Result<(usize, Vec<(String, String)>), String> {
        struct PreparedCard {
            card_id: uuid::Uuid,
            title: String,
            description: String,
            path: String,
            label: String,
            kind: StoryboardAssetKind,
            duration_frames: i64,
            media_frames: i64,
            workflow_inputs: std::collections::HashMap<String, StoryboardInputValue>,
        }

        let mut prepared = Vec::new();
        let mut skipped = Vec::new();

        for card in &self.storyboard_cards {
            let path = card.reference_path.trim();
            if path.is_empty() {
                skipped.push((card.title.clone(), "no generated asset".to_string()));
                continue;
            }
            let asset_kind = self.storyboard_detect_asset_kind(path, card.video_settings.as_ref());
            let duration_frames = self.storyboard_duration_to_frames(card.duration_seconds);
            if duration_frames <= 0 {
                skipped.push((card.title.clone(), "duration is zero".to_string()));
                continue;
            }
            let asset_row = match self.db.find_asset_by_path(&self.project_id, path) {
                Ok(row) => row,
                Err(err) => {
                    skipped.push((card.title.clone(), format!("asset lookup failed: {}", err)));
                    continue;
                }
            };
            let asset_src = asset_row
                .as_ref()
                .map(|asset| asset.src_abs.clone())
                .unwrap_or_else(|| path.to_string());
            if !std::path::Path::new(&asset_src).exists() {
                skipped.push((card.title.clone(), "asset file missing on disk".to_string()));
                continue;
            }
            let label = asset_row
                .as_ref()
                .map(|asset| asset.id.clone())
                .or_else(|| {
                    std::path::Path::new(&asset_src)
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                })
                .unwrap_or_else(|| card.title.clone());
            let media_frames = asset_row
                .as_ref()
                .and_then(|asset| asset.duration_frames)
                .filter(|frames| *frames > 0)
                .unwrap_or_else(|| match asset_kind {
                    StoryboardAssetKind::Image => 1,
                    StoryboardAssetKind::Video => duration_frames,
                });
            prepared.push(PreparedCard {
                card_id: card.id,
                title: card.title.clone(),
                description: card.description.clone(),
                path: asset_src,
                label,
                kind: asset_kind,
                duration_frames,
                media_frames,
                workflow_inputs: card.workflow_inputs.clone(),
            });
        }

        if prepared.is_empty() {
            return Err(
                "No storyboard cards with generated assets were ready for transfer.".to_string(),
            );
        }

        let mut video_tracks: Vec<timeline_crate::TrackId> = self
            .seq
            .graph
            .tracks
            .iter()
            .filter(|t| matches!(t.kind, timeline_crate::TrackKind::Video))
            .map(|t| t.id)
            .collect();

        if video_tracks.is_empty() {
            let track = timeline_crate::TrackBinding {
                id: timeline_crate::TrackId::new(),
                name: "Storyboard Video".to_string(),
                kind: timeline_crate::TrackKind::Video,
                node_ids: Vec::new(),
            };
            self::app_timeline::apply_timeline_command_impl(
                self,
                timeline_crate::TimelineCommand::UpsertTrack { track },
            )
            .map_err(|e| e.to_string())?;
            video_tracks = self
                .seq
                .graph
                .tracks
                .iter()
                .filter(|t| matches!(t.kind, timeline_crate::TrackKind::Video))
                .map(|t| t.id)
                .collect();
        }

        let mut track_occupancy: std::collections::HashMap<
            timeline_crate::TrackId,
            Vec<timeline_crate::FrameRange>,
        > = std::collections::HashMap::new();
        for track in &self.seq.graph.tracks {
            if !matches!(track.kind, timeline_crate::TrackKind::Video) {
                continue;
            }
            let mut ranges = Vec::new();
            for node_id in &track.node_ids {
                if let Some(node) = self.seq.graph.nodes.get(node_id) {
                    if let timeline_crate::TimelineNodeKind::Clip(clip) = &node.kind {
                        ranges.push(clip.timeline_range.clone());
                    }
                }
            }
            track_occupancy.insert(track.id, ranges);
        }

        let mut last_node_per_track: std::collections::HashMap<
            timeline_crate::TrackId,
            timeline_crate::NodeId,
        > = std::collections::HashMap::new();

        for track in &self.seq.graph.tracks {
            if let Some(last) = track.node_ids.last() {
                last_node_per_track.insert(track.id, *last);
            }
        }

        let mut overlay_index: usize = 1;

        let mut added = 0usize;
        let mut cursor = self.seq.duration_in_frames.max(0);

        for entry in prepared {
            let range_duration = entry.duration_frames.max(1);
            let timeline_range = timeline_crate::FrameRange::new(cursor, range_duration);
            let media_range = timeline_crate::FrameRange::new(0, entry.media_frames.max(1));
            let mut track_id = None;
            for candidate in &video_tracks {
                let occupied = track_occupancy.entry(*candidate).or_default();
                let overlaps = occupied.iter().any(|existing| {
                    let existing_end = existing.end();
                    let new_end = timeline_range.end();
                    timeline_range.start < existing_end && existing.start < new_end
                });
                if !overlaps {
                    track_id = Some(*candidate);
                    break;
                }
            }
            let track_id = match track_id {
                Some(id) => id,
                None => {
                    let new_track_id = self.ensure_storyboard_overlay_track(&mut overlay_index)?;
                    video_tracks.push(new_track_id);
                    track_occupancy.insert(new_track_id, Vec::new());
                    new_track_id
                }
            };
            track_occupancy
                .entry(track_id)
                .or_default()
                .push(timeline_range.clone());

            let mut clip_metadata_map = serde_json::Map::new();
            clip_metadata_map.insert(
                "source".to_string(),
                serde_json::Value::String("storyboard".to_string()),
            );
            clip_metadata_map.insert(
                "card_id".to_string(),
                serde_json::Value::String(entry.card_id.to_string()),
            );
            clip_metadata_map.insert(
                "card_title".to_string(),
                serde_json::Value::String(entry.title.clone()),
            );
            clip_metadata_map.insert(
                "card_description".to_string(),
                serde_json::Value::String(entry.description.clone()),
            );
            clip_metadata_map.insert(
                "reference_path".to_string(),
                serde_json::Value::String(entry.path.clone()),
            );

            let mut inputs_map = serde_json::Map::new();
            for (key, value) in entry.workflow_inputs.iter() {
                inputs_map.insert(key.clone(), value.to_json());
            }
            clip_metadata_map.insert(
                "workflow_inputs".to_string(),
                serde_json::Value::Object(inputs_map),
            );

            let clip = timeline_crate::ClipNode {
                asset_id: Some(entry.path.clone()),
                media_range,
                timeline_range,
                playback_rate: 1.0,
                reverse: false,
                metadata: serde_json::Value::Object(clip_metadata_map),
            };

            let mut node_metadata = serde_json::Map::new();
            node_metadata.insert(
                "source".to_string(),
                serde_json::Value::String("storyboard".to_string()),
            );
            node_metadata.insert(
                "card_id".to_string(),
                serde_json::Value::String(entry.card_id.to_string()),
            );
            node_metadata.insert(
                "asset_kind".to_string(),
                serde_json::Value::String(
                    match entry.kind {
                        StoryboardAssetKind::Video => "video",
                        StoryboardAssetKind::Image => "image",
                    }
                    .to_string(),
                ),
            );

            let node_id = timeline_crate::NodeId::new();
            let node = timeline_crate::TimelineNode {
                id: node_id,
                label: Some(entry.label.clone()),
                kind: timeline_crate::TimelineNodeKind::Clip(clip),
                locked: false,
                metadata: serde_json::Value::Object(node_metadata),
            };

            let mut edges = Vec::new();
            if let Some(prev_id) = last_node_per_track.get(&track_id) {
                edges.push(timeline_crate::TimelineEdge {
                    from: *prev_id,
                    to: node_id,
                    kind: timeline_crate::EdgeKind::Sequential,
                });
            }

            let placements = vec![timeline_crate::TrackPlacement {
                track_id,
                position: None,
            }];

            self::app_timeline::apply_timeline_command_impl(
                self,
                timeline_crate::TimelineCommand::InsertNode {
                    node,
                    placements,
                    edges,
                },
            )
            .map_err(|e| format!("failed to insert '{}' into timeline: {}", entry.title, e))?;

            last_node_per_track.insert(track_id, node_id);
            cursor = cursor.saturating_add(range_duration);
            added += 1;
        }

        if added == 0 {
            return Err("No storyboard cards were transferred into the timeline.".to_string());
        }

        Ok((added, skipped))
    }

    fn build_storyboard_prompt(
        &self,
        workflow_json: &str,
        card: &StoryboardCard,
        image_name: &str,
        resolved_file_inputs: &std::collections::HashMap<String, String>,
        raw_inputs: &std::collections::HashMap<String, StoryboardInputValue>,
        filename_prefix: &str,
    ) -> Result<(serde_json::Value, String), String> {
        let mut body_v: serde_json::Value =
            match serde_json::from_str::<serde_json::Value>(workflow_json) {
                Ok(v) => v,
                Err(e) => {
                    let converted = convert_workflow_to_prompt(workflow_json).map_err(|err| {
                        format!("Workflow JSON invalid ({e}); conversion also failed: {err}")
                    })?;
                    serde_json::from_str(&converted)
                        .map_err(|err| format!("Converted workflow parse failed: {}", err))?
                }
            };

        if body_v.get("prompt").is_none() {
            let converted = convert_workflow_to_prompt(workflow_json)
                .map_err(|e| format!("Workflow convert failed: {}", e))?;
            body_v = serde_json::from_str(&converted)
                .map_err(|e| format!("Converted workflow parse failed: {}", e))?;
        }

        let fps = {
            let num = self.seq.fps.num.max(1) as f32;
            let den = self.seq.fps.den.max(1) as f32;
            (num / den).max(1.0)
        };
        let fps_i = fps.round() as i64;
        let frames = (card.duration_seconds.max(0.1) * fps).round().max(1.0) as i64;

        if let Some(prompt_obj) = body_v.get_mut("prompt").and_then(|p| p.as_object_mut()) {
            crate::app_cloud::strip_ui_only_prompt_nodes(prompt_obj);
            self.patch_storyboard_prompt(
                prompt_obj,
                card,
                image_name,
                fps_i,
                frames,
                &filename_prefix,
                resolved_file_inputs,
                raw_inputs,
            );
        } else {
            return Err("Workflow prompt is not an object.".to_string());
        }

        let client_id = self.comfy_client_id.clone();
        if let serde_json::Value::Object(ref mut obj) = body_v {
            obj.insert(
                "client_id".into(),
                serde_json::Value::String(client_id.clone()),
            );
        }
        Ok((body_v, client_id))
    }

    fn patch_storyboard_prompt(
        &self,
        prompt: &mut serde_json::Map<String, serde_json::Value>,
        card: &StoryboardCard,
        image_name: &str,
        fps: i64,
        frames: i64,
        filename_prefix: &str,
        resolved_file_inputs: &std::collections::HashMap<String, String>,
        raw_inputs: &std::collections::HashMap<String, StoryboardInputValue>,
    ) {
        let format_override = card.video_settings.as_ref().and_then(|s| {
            let trimmed = s.format.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        });
        let codec_override = card.video_settings.as_ref().and_then(|s| {
            let trimmed = s.codec.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        });

        for node in prompt.values_mut() {
            if let Some(obj) = node.as_object_mut() {
                let mut title_lower = String::new();
                if let Some(meta) = obj.get("_meta").and_then(|m| m.as_object()) {
                    if let Some(title) = meta.get("title").and_then(|t| t.as_str()) {
                        title_lower = title.to_lowercase();
                    }
                }
                if let Some(inputs) = obj.get_mut("inputs").and_then(|i| i.as_object_mut()) {
                    if inputs.contains_key("filename_prefix") {
                        inputs.insert(
                            "filename_prefix".into(),
                            serde_json::Value::String(filename_prefix.to_string()),
                        );
                    }
                    if let Some(val) = inputs.get_mut("image") {
                        if matches!(val, serde_json::Value::Null | serde_json::Value::String(_)) {
                            *val = serde_json::Value::String(image_name.to_string());
                        }
                    }
                    if let Some(val) = inputs.get_mut("image_path") {
                        if matches!(val, serde_json::Value::Null | serde_json::Value::String(_)) {
                            *val = serde_json::Value::String(image_name.to_string());
                        }
                    }
                    if inputs.contains_key("frame_rate") {
                        inputs.insert("frame_rate".into(), serde_json::Value::Number(fps.into()));
                    }
                    if inputs.contains_key("fps") {
                        inputs.insert("fps".into(), serde_json::Value::Number(fps.into()));
                    }
                    if inputs.contains_key("length") {
                        inputs.insert("length".into(), serde_json::Value::Number(frames.into()));
                    }
                    if let Some(fmt) = format_override.clone() {
                        if inputs.contains_key("format") {
                            inputs.insert("format".into(), serde_json::Value::String(fmt));
                        }
                    }
                    if let Some(codec) = codec_override.clone() {
                        if inputs.contains_key("codec") {
                            inputs.insert("codec".into(), serde_json::Value::String(codec));
                        }
                    }
                    if title_lower.contains("positive")
                        && !card.description.trim().is_empty()
                        && inputs.contains_key("text")
                    {
                        inputs.insert(
                            "text".into(),
                            serde_json::Value::String(card.description.trim().to_string()),
                        );
                    }
                }
            }
        }

        for (map_key, value) in raw_inputs {
            let (node_id, input_key) = match map_key.split_once(':') {
                Some(parts) => parts,
                None => continue,
            };
            let Some(node) = prompt.get_mut(node_id).and_then(|n| n.as_object_mut()) else {
                continue;
            };
            let Some(inputs) = node.get_mut("inputs").and_then(|i| i.as_object_mut()) else {
                continue;
            };
            match value {
                StoryboardInputValue::File(path) => {
                    let override_value = resolved_file_inputs
                        .get(map_key)
                        .cloned()
                        .unwrap_or_else(|| path.clone());
                    if !override_value.trim().is_empty() {
                        inputs.insert(input_key.into(), serde_json::Value::String(override_value));
                    }
                }
                other => {
                    inputs.insert(input_key.into(), other.to_json());
                }
            }
        }
    }

    pub(crate) fn storyboard_forced_filename_prefix(&self, card_id: uuid::Uuid) -> String {
        let project_component = Self::sanitize_filename_component(&self.project_id);
        let short_id: String = card_id.to_string().chars().take(8).collect();
        format!("storyboard-{}-{}", project_component, short_id)
    }

    pub(crate) fn sanitize_filename_component(input: &str) -> String {
        let mut s: String = input
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        while s.contains("--") {
            s = s.replace("--", "-");
        }
        s = s.trim_matches('-').to_string();
        if s.is_empty() {
            "storyboard".to_string()
        } else {
            s
        }
    }

    fn log_comfy_embed(&mut self, message: impl Into<String>) {
        self.comfy_embed_logs.push_back(message.into());
        while self.comfy_embed_logs.len() > 200 {
            self.comfy_embed_logs.pop_front();
        }
    }

    fn close_comfy_embed_host(&mut self, reason: &str) {
        if let Some(mut host) = self.comfy_webview.take() {
            host.close();
            self.log_comfy_embed(reason.to_string());
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn file_dialog(&self) -> rfd::FileDialog {
        if let (Some(window), Some(display)) = (self.raw_window_handle, self.raw_display_handle) {
            let parent = FileDialogParent { window, display };
            rfd::FileDialog::new().set_parent(&parent)
        } else {
            rfd::FileDialog::new()
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn file_dialog(&self) -> rfd::FileDialog {
        rfd::FileDialog::new()
    }

    #[cfg(target_os = "macos")]
    fn storyboard_decode_video_fallback(
        path: &std::path::Path,
        t_sec: f64,
    ) -> Option<media_io::YuvFrame> {
        use media_io::YuvPixFmt;

        let config = native_decoder::DecoderConfig {
            hardware_acceleration: true,
            preferred_format: Some(native_decoder::YuvPixFmt::Nv12),
            zero_copy: false,
        };
        let mut decoder = native_decoder::create_decoder(path, config).ok()?;

        let _ = decoder.seek_to(t_sec);
        decoder.set_strict_paused(false);

        let mut frame = decoder.decode_frame(t_sec).ok().flatten();
        let mut attempts = 0;
        while frame.is_none() && attempts < 12 {
            let _ = decoder.decode_frame(t_sec);
            attempts += 1;
            frame = decoder.decode_frame(t_sec).ok().flatten();
        }

        if let Some(frame) = frame {
            let fmt = match frame.format {
                native_decoder::YuvPixFmt::Nv12 => YuvPixFmt::Nv12,
                native_decoder::YuvPixFmt::P010 => YuvPixFmt::P010,
            };

            Some(media_io::YuvFrame {
                fmt,
                y: frame.y_plane,
                uv: frame.uv_plane,
                width: frame.width,
                height: frame.height,
            })
        } else {
            Self::storyboard_decode_video_ffmpeg(path, t_sec)
        }
    }

    #[cfg(target_os = "macos")]
    fn storyboard_decode_video_ffmpeg(
        path: &std::path::Path,
        t_sec: f64,
    ) -> Option<media_io::YuvFrame> {
        use media_io::YuvPixFmt;

        let info = media_io::probe_media(path).ok()?;
        let w = info.width?;
        let h = info.height?;

        let output = std::process::Command::new("ffmpeg")
            .arg("-loglevel")
            .arg("error")
            .arg("-ss")
            .arg(format!("{:.3}", t_sec.max(0.0)))
            .arg("-i")
            .arg(path)
            .arg("-frames:v")
            .arg("1")
            .arg("-f")
            .arg("rawvideo")
            .arg("-pix_fmt")
            .arg("nv12")
            .arg("-")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }

        let y_sz = (w as usize) * (h as usize);
        let uv_sz = y_sz / 2;
        if output.stdout.len() < y_sz + uv_sz {
            return None;
        }

        let y = output.stdout[..y_sz].to_vec();
        let uv = output.stdout[y_sz..y_sz + uv_sz].to_vec();

        Some(media_io::YuvFrame {
            fmt: YuvPixFmt::Nv12,
            y,
            uv,
            width: w,
            height: h,
        })
    }

    // cloud/project methods moved to their modules
    fn new(db: ProjectDb) -> Self {
        let project_id = "default".to_string();
        let _ = db.ensure_project(&project_id, "Default Project", None);
        let mut seq = Sequence::new("Main", 1920, 1080, Fps::new(30, 1), 600);
        if seq.tracks.is_empty() {
            // Default to three video and three audio tracks
            for i in 1..=3 {
                seq.add_track(Track {
                    name: format!("V{}", i),
                    items: vec![],
                });
            }
            for i in 1..=3 {
                seq.add_track(Track {
                    name: format!("A{}", i),
                    items: vec![],
                });
            }
        }
        seq.graph = timeline_crate::migrate_sequence_tracks(&seq);
        let db_path = db.path().to_path_buf();
        let storage_root = db
            .get_project_base_path(&project_id)
            .ok()
            .flatten()
            .or_else(|| db_path.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(project::app_data_dir);
        let optimized_root = storage_root.join("media").join("optimized");
        let default_cache_max = std::cmp::max(1, num_cpus::get().saturating_sub(1));
        let cache_max = std::env::var("GAUSIAN_CACHE_MAX_JOBS")
            .ok()
            .and_then(|raw| raw.parse::<usize>().ok())
            .map(|val| val.max(1))
            .unwrap_or(default_cache_max);
        let cache_manager = match CacheManager::new(optimized_root.clone(), cache_max) {
            Ok(manager) => manager,
            Err(err) => {
                let fallback = std::env::temp_dir()
                    .join("gausian_native")
                    .join("optimized_media");
                tracing::warn!(
                    target = "cache",
                    error = %err,
                    fallback = %fallback.display(),
                    "failed to initialize optimized cache at {}; using fallback directory",
                    optimized_root.display()
                );
                CacheManager::new(fallback, cache_max)
                    .expect("failed to initialize optimized cache fallback directory")
            }
        };
        let cache_events = cache_manager.subscribe();
        let jobs_handle = jobs_crate::JobsRuntime::start(db_path.clone(), 2);
        let hardware_caps = Arc::new(crate::media_info::detect_hardware_caps());
        let (proxy_queue, proxy_events_rx) =
            crate::proxy_queue::ProxyQueue::start(db_path.clone(), hardware_caps.clone());
        let (screenplay_tx, screenplay_rx) = unbounded();
        let (comfy_ingest_tx, comfy_ingest_rx) = unbounded::<(String, std::path::PathBuf)>();
        let (comfy_ws_tx, comfy_ws_rx) = unbounded::<ComfyWsEvent>();
        let comfy_http_agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(3))
            .timeout_read(Duration::from_secs(10))
            .timeout_write(Duration::from_secs(3))
            .build();
        let comfy_client_id = uuid::Uuid::new_v4().to_string();
        let mut app = Self {
            db,
            project_id,
            import_path: String::new(),
            seq,
            timeline_history: CommandHistory::default(),
            zoom_px_per_frame: 2.0,
            playhead: 0,
            playing: false,
            last_tick: None,
            play_anchor_instant: None,
            play_anchor_frame: 0,
            preview: PreviewState::new(),
            audio_out: audio_engine::AudioEngine::new().ok(),
            selected: None,
            drag: None,
            export: ExportUiState::default(),
            import_workers: Vec::new(),
            jobs: Some(jobs_handle),
            job_events: Vec::new(),
            show_jobs: false,
            hardware_caps,
            proxy_queue: Some(proxy_queue),
            proxy_events: Some(proxy_events_rx),
            proxy_status: std::collections::HashMap::new(),
            proxy_logs: std::collections::HashMap::new(),
            proxy_mode_user: ProxyMode::OriginalOptimized,
            proxy_mode_override: None,
            proxy_preview_overrides: std::collections::HashSet::new(),
            auto_proxy_setting: AutoProxySetting::LargeOnly,
            cache_manager,
            cache_events,
            cache_job_status: std::collections::HashMap::new(),
            viewer_scale: ViewerScale::Full,
            playback_lag_frames: 0,
            playback_stable_frames: 0,
            asset_cache: std::collections::HashMap::new(),
            auto_proxy_requests: std::collections::HashSet::new(),
            auto_analysis_requests: std::collections::HashSet::new(),
            pending_heavy_assets: std::collections::VecDeque::new(),
            pending_heavy_asset_set: std::collections::HashSet::new(),
            last_heavy_job_dispatch: None,
            decode_mgr: DecodeManager::default(),
            playback_clock: PlaybackClock { rate: 1.0, ..Default::default() },
            audio_cache: AudioCache::default(),
            audio_buffers: AudioBufferCache::default(),
            last_preview_key: None,
            engine: EngineState { state: PlayState::Paused, rate: 1.0, target_pts: 0.0 },
            last_sent: None,
            last_seek_sent_pts: None,
            last_play_reanchor_time: None,
            strict_pause: true,
            last_seek_request_at: None,
            last_present_pts: None,
            preview_last_capture: None,
            settings: PreviewSettings::default(),
            show_settings: false,
            show_screenplay_panel: false,
            screenplay_api_token: String::new(),
            screenplay_provider: crate::screenplay::ProviderKind::Mock,
            screenplay_model: "mock-screenwriter".to_string(),
            screenplay_active_tab: ScreenplayTab::Conversation,
            screenplay_session: None,
            screenplay_questions: Vec::new(),
            screenplay_input: String::new(),
            screenplay_revision_input: String::new(),
            screenplay_revision_scope: crate::screenplay::RevisionScope::EntireDraft,
            screenplay_error: None,
            screenplay_logs: std::collections::VecDeque::with_capacity(16),
            screenplay_busy: false,
            screenplay_generate_busy: false,
            screenplay_cancel_requested: false,
            screenplay_session_handle: None,
            screenplay_event_tx: screenplay_tx.clone(),
            screenplay_event_rx: screenplay_rx,
            comfy: crate::comfyui::ComfyUiManager::default(),
            show_comfy_panel: false,
            comfy_repo_input: String::new(),
            comfy_install_dir_input: crate::comfyui::ComfyUiManager::default_install_dir()
                .to_string_lossy()
                .to_string(),
            comfy_torch_backend: crate::comfyui::TorchBackend::Auto,
            comfy_venv_python_input: String::new(),
            comfy_recreate_venv: false,
            comfy_install_ffmpeg: true,
            comfy_ws_monitor: false,
            comfy_ws_thread: None,
            comfy_ws_stop: None,
            comfy_api_key: String::new(),
            modal_enabled: true,
            modal_base_url: String::new(),
            modal_api_key: String::new(),
            modal_payload: String::from("{\n  \"workflow\": \"basic-video\",\n  \"params\": { \"width\": 1920, \"height\": 1080, \"fps\": 30, \"seconds\": 5 }\n}"),
            modal_logs: std::collections::VecDeque::with_capacity(256),
            modal_rx: {
                let (_tx, rx) = unbounded();
                rx
            },
            modal_tx: {
                let (tx, _rx) = unbounded();
                tx
            },
            cloud_target: CloudTarget::Prompt,
            modal_relay_ws_url: String::new(),
            modal_queue_pending: 0,
            modal_queue_running: 0,
            modal_job_progress: std::collections::HashMap::new(),
            modal_job_source: std::collections::HashMap::new(),
            modal_phase_plans: std::collections::HashMap::new(),
            modal_phase_agg: std::collections::HashMap::new(),
            modal_active_job: std::sync::Arc::new(std::sync::Mutex::new(None)),
            modal_job_prefixes: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            modal_recent: Vec::new(),
            modal_monitor_requested: false,
            modal_last_progress_at: None,
            modal_known_jobs: std::collections::HashSet::new(),
            pip_index_url_input: String::new(),
            pip_extra_index_url_input: String::new(),
            pip_trusted_hosts_input: String::new(),
            pip_proxy_input: String::new(),
            pip_no_cache: false,
            // Default to not opening ComfyUI inside the editor
            comfy_embed_inside: false,
            comfy_webview: None,
            comfy_embed_logs: std::collections::VecDeque::with_capacity(128),
            comfy_embed_in_assets: true,
            comfy_assets_height: 320.0,
            show_comfy_view_window: true,
            comfy_auto_import: true,
            comfy_import_logs: std::collections::VecDeque::with_capacity(256),
            comfy_client_id,
            comfy_jobs: std::collections::HashMap::new(),
            comfy_known_prompts: std::collections::HashSet::new(),
            comfy_queue_pending: 0,
            comfy_queue_running: 0,
            comfy_last_queue_poll: None,
            comfy_ws_rx,
            comfy_ws_tx,
            comfy_http_agent,
            comfy_storyboard_jobs: std::collections::HashMap::new(),
            comfy_prompt_to_card: std::collections::HashMap::new(),
            comfy_alerts: std::collections::VecDeque::with_capacity(8),
            #[cfg(not(target_arch = "wasm32"))]
            raw_window_handle: None,
            #[cfg(not(target_arch = "wasm32"))]
            raw_display_handle: None,
            comfy_ingest_thread: None,
            comfy_ingest_stop: None,
            comfy_ingest_rx,
            comfy_ingest_tx,
            comfy_ingest_project_id: None,
            show_projects: false,
            new_project_name: String::new(),
            new_project_base: String::new(),
            mode: AppMode::ProjectPicker,
            workspace_view: WorkspaceView::Timeline,
            chat_messages: Vec::new(),
            chat_input: String::new(),
            chat_busy: false,
            chat_error: None,
            chat_model: String::from("llama3.2:latest"),
            chat_base_url: String::from("http://localhost:11434"),
            chat_system_prompt: String::from("You are a helpful assistant for the Gausian native video editor. Provide concise, trustworthy answers about storytelling, creative direction, and the technical workflow of this application."),
            chat_temperature: 0.7,
            chat_event_tx: {
                let (tx, _rx) = unbounded();
                tx
            },
            chat_event_rx: {
                let (_tx, rx) = unbounded();
                rx
            },
            storyboard_cards: Vec::new(),
            storyboard_selected: None,
            storyboard_previews: std::collections::HashMap::new(),
            storyboard_input_previews: std::collections::HashMap::new(),
            storyboard_preview_resets: std::collections::HashSet::new(),
            storyboard_pending_input_refresh: std::collections::HashMap::new(),
            storyboard_workflows: Vec::new(),
            storyboard_batch_busy: false,
            last_save_at: None,
            asset_thumb_textures: std::collections::HashMap::new(),
            textures_to_free_next_frame: Vec::new(),
            dragging_asset: None,
            asset_thumb_w: 148.0,
            assets_drop_rect: None,
            timeline_drop_rect: None,
            pending_timeline_drops: Vec::new(),
        };
        let (chat_tx, chat_rx) = unbounded();
        app.chat_event_tx = chat_tx;
        app.chat_event_rx = chat_rx;
        // Modal events channel
        let (mtx, mrx) = unbounded();
        app.modal_tx = mtx;
        app.modal_rx = mrx;
        app.load_proxy_settings();
        app.load_comfy_settings();
        // Initialize ComfyUI repo input from current config (if any)
        if let Some(p) = app.comfy.config().repo_path.as_ref() {
            app.comfy_repo_input = p.to_string_lossy().to_string();
        }
        app.refresh_storyboard_workflows();
        app.load_storyboard_from_settings();
        app.sync_tracks_from_graph();
        app
    }

    // asset/timeline helpers moved to their modules
}

// Best-effort converter from a generic "workflow" JSON into a ComfyUI /prompt payload.
// This is intentionally conservative: it tries to recognize a "nodes" array and
// build a minimal prompt map with class_type and any provided literal inputs.
// Complex graph links are not guaranteed to convert; if conversion isn't possible,
// returns an Err with a helpful message.
fn convert_workflow_to_prompt(workflow_json: &str) -> Result<String, String> {
    app_cloud::convert_workflow_to_prompt(workflow_json)
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.textures_to_free_next_frame.clear();
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.raw_window_handle = frame.window_handle().ok().map(|h| h.as_raw());
            self.raw_display_handle = frame.display_handle().ok().map(|h| h.as_raw());
        }
        while let Ok(ev) = self.cache_events.try_recv() {
            self.handle_cache_event(ev);
        }
        while let Ok(ev) = self.screenplay_event_rx.try_recv() {
            self.screenplay_handle_event(ev);
        }
        while let Ok(ev) = self.chat_event_rx.try_recv() {
            self.chat_handle_event(ev);
        }
        self.prune_comfy_alerts();
        self.process_pending_heavy_assets();
        // Drain modal events and append to logs
        while let Ok(ev) = self.modal_rx.try_recv() {
            match ev {
                ModalEvent::Log(s) => {
                    self.modal_logs.push_back(s);
                    while self.modal_logs.len() > 256 {
                        self.modal_logs.pop_front();
                    }
                }
                ModalEvent::JobQueued(id) => {
                    self.modal_logs.push_back(format!("Queued job: {}", id));
                    // Build a phase plan from current payload for this job id
                    let plan = Self::compute_phase_plan_from_payload(&self.modal_payload);
                    self.modal_phase_plans.insert(id.clone(), plan);
                    self.modal_phase_agg
                        .entry(id.clone())
                        .or_insert_with(PhaseAgg::default);
                    // Ensure monitor is requested when a job is queued
                    self.modal_monitor_requested = true;
                    // Track as known job this session
                    self.modal_known_jobs.insert(id.clone());
                }
                ModalEvent::JobQueuedWithPrefix(id, prefix) => {
                    // Mark this as the active job and clear previous progress bars
                    if let Ok(mut a) = self.modal_active_job.lock() {
                        *a = Some(id.clone());
                    }
                    self.modal_phase_agg.clear();
                    self.modal_job_progress.clear();
                    self.modal_phase_plans.clear();
                    // Also prune known jobs and prefix map to only this id
                    self.modal_known_jobs.clear();
                    self.modal_known_jobs.insert(id.clone());
                    if let Ok(mut m) = self.modal_job_prefixes.lock() {
                        m.retain(|k, _| k == &id);
                    }
                    // Seed phase plan and an empty aggregate entry so the UI shows a placeholder bar immediately
                    let plan = Self::compute_phase_plan_from_payload(&self.modal_payload);
                    self.modal_phase_plans.insert(id.clone(), plan);
                    self.modal_phase_agg.insert(id.clone(), PhaseAgg::default());
                    // Remember expected output filename prefix for this job
                    if let Ok(mut m) = self.modal_job_prefixes.lock() {
                        m.insert(id.clone(), prefix.clone());
                    }
                    // Lightweight per-job poller for /jobs/{id} to drive progress/import without /progress-status
                    let http_base = {
                        let mut base = self.modal_base_url.trim().to_string();
                        if !base.starts_with("http://") && !base.starts_with("https://") {
                            base = format!("https://{}", base);
                        }
                        if base.ends_with("/health") {
                            base = base[..base.len() - "/health".len()]
                                .trim_end_matches('/')
                                .to_string();
                        }
                        if base.ends_with("/healthz") {
                            base = base[..base.len() - "/healthz".len()]
                                .trim_end_matches('/')
                                .to_string();
                        }
                        base
                    };
                    // Re-introduce a lightweight /jobs/{id} poller while the job is running
                    // to keep progress fresh when WS events are sparse. It stops as soon as
                    // the job is completed to avoid any cold starts.
                    let token = self.modal_api_key.clone();
                    let jid = id.clone();
                    let tx_log = self.modal_tx.clone();
                    let tx_import = self.comfy_ingest_tx.clone();
                    let proj_id = self.project_id.clone();
                    let app_tmp = project::app_data_dir().join("tmp").join("cloud");
                    let _ = std::fs::create_dir_all(&app_tmp);
                    let active_job = self.modal_active_job.clone();
                    let job_prefixes = self.modal_job_prefixes.clone();
                    std::thread::spawn(move || {
                        use std::time::Duration;
                        loop {
                            // Exit if a different job became active or job cleared
                            let still_active = active_job
                                .lock()
                                .ok()
                                .and_then(|a| a.clone())
                                .map(|cur| cur == jid)
                                .unwrap_or(false);
                            if !still_active {
                                break;
                            }
                            // Poll job state
                            let job_url =
                                format!("{}/jobs/{}", http_base.trim_end_matches('/'), jid);
                            let mut req = ureq::get(&job_url);
                            if !token.trim().is_empty() {
                                req = req.set("Authorization", &format!("Bearer {}", token));
                            }
                            if let Ok(resp) = req.call() {
                                if let Ok(body) = resp.into_string() {
                                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body)
                                    {
                                        let pr = v
                                            .get("progress_percent")
                                            .and_then(|x| x.as_f64())
                                            .unwrap_or(0.0)
                                            as f32;
                                        let cur = v
                                            .get("current_step")
                                            .and_then(|x| x.as_u64())
                                            .unwrap_or(0)
                                            as u32;
                                        let tot = v
                                            .get("total_steps")
                                            .and_then(|x| x.as_u64())
                                            .unwrap_or(0)
                                            as u32;
                                        let _ = tx_log.send(ModalEvent::CloudProgress {
                                            job_id: jid.clone(),
                                            progress: pr,
                                            current: cur,
                                            total: tot,
                                            node_id: None,
                                        });
                                        let _ = tx_log.send(ModalEvent::CloudSource {
                                            job_id: jid.clone(),
                                            source: crate::CloudUpdateSrc::Jobs,
                                        });
                                        // If no totals yet, try the headless /progress-status for richer details
                                        if (tot == 0) || (pr == 0.0 && cur == 0) {
                                            let status_url = format!(
                                                "{}/progress-status",
                                                http_base.trim_end_matches('/')
                                            );
                                            let mut sreq = ureq::get(&status_url);
                                            if !token.trim().is_empty() {
                                                sreq = sreq.set(
                                                    "Authorization",
                                                    &format!("Bearer {}", token),
                                                );
                                            }
                                            if let Ok(sresp) = sreq.call() {
                                                if let Ok(sbody) = sresp.into_string() {
                                                    if let Ok(sv) =
                                                        serde_json::from_str::<serde_json::Value>(
                                                            &sbody,
                                                        )
                                                    {
                                                        if let Some(arr) = sv
                                                            .get("job_details")
                                                            .and_then(|a| a.as_array())
                                                        {
                                                            for it in arr {
                                                                let sid = it
                                                                    .get("job_id")
                                                                    .and_then(|s| s.as_str())
                                                                    .unwrap_or("");
                                                                if sid == jid {
                                                                    let spr = it
                                                                        .get("progress_percent")
                                                                        .and_then(|x| x.as_f64())
                                                                        .unwrap_or(0.0)
                                                                        as f32;
                                                                    let scur = it
                                                                        .get("current_step")
                                                                        .and_then(|x| x.as_u64())
                                                                        .unwrap_or(0)
                                                                        as u32;
                                                                    let stot = it
                                                                        .get("total_steps")
                                                                        .and_then(|x| x.as_u64())
                                                                        .unwrap_or(0)
                                                                        as u32;
                                                                    let _ = tx_log.send(
                                                                        ModalEvent::CloudProgress {
                                                                            job_id: jid.clone(),
                                                                            progress: spr,
                                                                            current: scur,
                                                                            total: stot,
                                                                            node_id: None,
                                                                        },
                                                                    );
                                                                    let _ = tx_log.send(ModalEvent::CloudSource { job_id: jid.clone(), source: crate::CloudUpdateSrc::Status });
                                                                    break;
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        // Stop polling promptly when completed; optionally import artifacts
                                        let status =
                                            v.get("status").and_then(|s| s.as_str()).unwrap_or("");
                                        if status == "completed" {
                                            // Only import if still the active job (avoid double-import if WS already handled it)
                                            let still_active2 = active_job
                                                .lock()
                                                .ok()
                                                .and_then(|a| a.clone())
                                                .map(|cur| cur == jid)
                                                .unwrap_or(false);
                                            if still_active2 {
                                                let prefix_opt = job_prefixes
                                                    .lock()
                                                    .ok()
                                                    .and_then(|m| m.get(&jid).cloned());
                                                let _ = tx_log
                                                    .send(ModalEvent::JobImporting(jid.clone()));

                                                let mut candidates: Vec<(String, String)> =
                                                    Vec::new();
                                                Self::collect_modal_artifacts(
                                                    &mut candidates,
                                                    v.get("artifacts")
                                                        .and_then(|a| a.as_array())
                                                        .cloned(),
                                                );

                                                if candidates.is_empty() {
                                                    let art_url = format!(
                                                        "{}/artifacts/{}",
                                                        http_base.trim_end_matches('/'),
                                                        jid
                                                    );
                                                    let mut areq = ureq::get(&art_url);
                                                    if !token.trim().is_empty() {
                                                        areq = areq.set(
                                                            "Authorization",
                                                            &format!("Bearer {}", token),
                                                        );
                                                    }
                                                    if let Ok(aresp) = areq.call() {
                                                        if let Ok(abody) = aresp.into_string() {
                                                            if let Ok(av) = serde_json::from_str::<
                                                                serde_json::Value,
                                                            >(
                                                                &abody
                                                            ) {
                                                                Self::collect_modal_artifacts(
                                                                    &mut candidates,
                                                                    av.get("artifacts")
                                                                        .and_then(|a| a.as_array())
                                                                        .cloned(),
                                                                );
                                                            }
                                                        }
                                                    }
                                                }

                                                if candidates.is_empty() {
                                                    let mut tries = 0u8;
                                                    while tries < 5 && candidates.is_empty() {
                                                        std::thread::sleep(
                                                            std::time::Duration::from_millis(300),
                                                        );
                                                        let job_url = format!(
                                                            "{}/jobs/{}",
                                                            http_base.trim_end_matches('/'),
                                                            jid
                                                        );
                                                        let mut req2 = ureq::get(&job_url);
                                                        if !token.trim().is_empty() {
                                                            req2 = req2.set(
                                                                "Authorization",
                                                                &format!("Bearer {}", token),
                                                            );
                                                        }
                                                        if let Ok(resp2) = req2.call() {
                                                            if let Ok(body2) = resp2.into_string() {
                                                                if let Ok(v2) = serde_json::from_str::<
                                                                    serde_json::Value,
                                                                >(
                                                                    &body2
                                                                ) {
                                                                    Self::collect_modal_artifacts(
                                                                        &mut candidates,
                                                                        v2.get("artifacts")
                                                                            .and_then(|a| {
                                                                                a.as_array()
                                                                            })
                                                                            .cloned(),
                                                                    );
                                                                }
                                                            }
                                                        }
                                                        tries += 1;
                                                    }
                                                }

                                                if candidates.is_empty() {
                                                    let hz_url = format!(
                                                        "{}/healthz",
                                                        http_base.trim_end_matches('/')
                                                    );
                                                    let mut hreq = ureq::get(&hz_url);
                                                    if !token.trim().is_empty() {
                                                        hreq = hreq.set(
                                                            "Authorization",
                                                            &format!("Bearer {}", token),
                                                        );
                                                    }
                                                    if let Ok(hresp) = hreq.call() {
                                                        if let Ok(hbody) = hresp.into_string() {
                                                            if let Ok(hv) = serde_json::from_str::<
                                                                serde_json::Value,
                                                            >(
                                                                &hbody
                                                            ) {
                                                                if let Some(recent) = hv
                                                                    .get("recent")
                                                                    .and_then(|a| a.as_array())
                                                                {
                                                                    for job in recent {
                                                                        if job.get("id").and_then(
                                                                            |s| s.as_str(),
                                                                        ) == Some("outputs")
                                                                        {
                                                                            if let Some(arts) = job
                                                                                .get("artifacts")
                                                                                .and_then(|a| {
                                                                                    a.as_array()
                                                                                })
                                                                            {
                                                                                Self::collect_modal_artifacts(
                                                                                    &mut candidates,
                                                                                    Some(arts.to_vec()),
                                                                                );
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }

                                                let mut seen_names: std::collections::HashSet<
                                                    String,
                                                > = std::collections::HashSet::new();
                                                let mut downloaded: Vec<String> = Vec::new();
                                                let mut ack_names: Vec<String> = Vec::new();

                                                let mut attempt_download =
                                                    |orig_name: &str, url: &str| -> bool {
                                                        let base_name =
                                                            std::path::Path::new(orig_name)
                                                                .file_name()
                                                                .and_then(|s| s.to_str())
                                                                .unwrap_or(orig_name);
                                                        if !seen_names.insert(base_name.to_string())
                                                        {
                                                            return false;
                                                        }
                                                        let mut req = ureq::get(url);
                                                        if !token.trim().is_empty() {
                                                            req = req.set(
                                                                "Authorization",
                                                                &format!("Bearer {}", token),
                                                            );
                                                        }
                                                        match req.call() {
                                                            Ok(resp) => {
                                                                let mut reader = resp.into_reader();
                                                                let dest = app_tmp.join(base_name);
                                                                if let Some(parent) = dest.parent()
                                                                {
                                                                    let _ = std::fs::create_dir_all(
                                                                        parent,
                                                                    );
                                                                }
                                                                match std::fs::File::create(&dest) {
                                                                    Ok(mut f) => {
                                                                        if std::io::copy(
                                                                            &mut reader,
                                                                            &mut f,
                                                                        )
                                                                        .is_ok()
                                                                        {
                                                                            let _ =
                                                                                tx_import.send((
                                                                                    proj_id.clone(),
                                                                                    dest,
                                                                                ));
                                                                            downloaded.push(
                                                                                base_name
                                                                                    .to_string(),
                                                                            );
                                                                            ack_names.push(
                                                                                orig_name
                                                                                    .to_string(),
                                                                            );
                                                                            let _ = tx_log.send(
                                                                                ModalEvent::Log(
                                                                                    format!(
                                                                            "Downloaded {}",
                                                                            base_name
                                                                        ),
                                                                                ),
                                                                            );
                                                                            true
                                                                        } else {
                                                                            let _ = tx_log.send(ModalEvent::Log(format!(
                                                                            "Download failed (write) {}",
                                                                            base_name
                                                                        )));
                                                                            false
                                                                        }
                                                                    }
                                                                    Err(e) => {
                                                                        let _ = tx_log.send(ModalEvent::Log(format!(
                                                                        "Download failed (create) {}: {}",
                                                                        base_name, e
                                                                    )));
                                                                        false
                                                                    }
                                                                }
                                                            }
                                                            Err(e) => {
                                                                let _ = tx_log.send(
                                                                    ModalEvent::Log(format!(
                                                                        "Download failed {}: {}",
                                                                        base_name, e
                                                                    )),
                                                                );
                                                                false
                                                            }
                                                        }
                                                    };

                                                // Prefer strict prefix matches when available; otherwise fall back to any mp4 candidate
                                                let strict_only = if let Some(pref) =
                                                    prefix_opt.as_ref()
                                                {
                                                    let mut c = 0usize;
                                                    for (name, _url) in candidates.iter() {
                                                        let base_name = std::path::Path::new(name)
                                                            .file_name()
                                                            .and_then(|s| s.to_str())
                                                            .unwrap_or(name);
                                                        if base_name.starts_with(pref) {
                                                            c += 1;
                                                        }
                                                    }
                                                    c > 0
                                                } else {
                                                    false
                                                };

                                                for (name, url) in candidates.iter() {
                                                    if strict_only {
                                                        if let Some(pref) = prefix_opt.as_ref() {
                                                            let base_name =
                                                                std::path::Path::new(name)
                                                                    .file_name()
                                                                    .and_then(|s| s.to_str())
                                                                    .unwrap_or(name);
                                                            if !base_name.starts_with(pref) {
                                                                continue;
                                                            }
                                                        }
                                                    }
                                                    let _ = attempt_download(name, url);
                                                }

                                                if downloaded.is_empty() {
                                                    // Fallback: attempt canonical /view/{job_id}
                                                    let view_url = format!(
                                                        "{}/view/{}",
                                                        http_base.trim_end_matches('/'),
                                                        jid
                                                    );
                                                    let mut vreq = ureq::get(&view_url);
                                                    if !token.trim().is_empty() {
                                                        vreq = vreq.set(
                                                            "Authorization",
                                                            &format!("Bearer {}", token),
                                                        );
                                                    }
                                                    match vreq.call() {
                                                        Ok(vresp) => {
                                                            let mut reader = vresp.into_reader();
                                                            let fallback_name = if let Some(pref) =
                                                                prefix_opt.as_ref()
                                                            {
                                                                format!("{}-view.mp4", pref)
                                                            } else {
                                                                format!("{}.mp4", jid)
                                                            };
                                                            let tmp = app_tmp.join(&fallback_name);
                                                            if let Some(parent) = tmp.parent() {
                                                                let _ =
                                                                    std::fs::create_dir_all(parent);
                                                            }
                                                            if let Ok(mut f) =
                                                                std::fs::File::create(&tmp)
                                                            {
                                                                let _ = std::io::copy(
                                                                    &mut reader,
                                                                    &mut f,
                                                                );
                                                                let _ = tx_import.send((
                                                                    proj_id.clone(),
                                                                    tmp.clone(),
                                                                ));
                                                                let _ = tx_log.send(ModalEvent::Log(format!(
                                                                    "Downloaded via /view/{{job_id}} → queued import: {}",
                                                                    tmp.to_string_lossy()
                                                                )));
                                                            }
                                                        }
                                                        Err(_) => {
                                                            let _ = tx_log.send(ModalEvent::Log(format!(
                                                                "No downloadable artifacts found for {}",
                                                                jid
                                                            )));
                                                        }
                                                    }
                                                } else {
                                                    ack_names.sort();
                                                    ack_names.dedup();
                                                    let ack_url = format!(
                                                        "{}/jobs/{}/imported",
                                                        http_base.trim_end_matches('/'),
                                                        jid
                                                    );
                                                    let mut preq = ureq::post(&ack_url)
                                                        .set("Content-Type", "application/json");
                                                    if !token.trim().is_empty() {
                                                        preq = preq.set(
                                                            "Authorization",
                                                            &format!("Bearer {}", token),
                                                        );
                                                    }
                                                    let body = serde_json::json!({ "filenames": ack_names });
                                                    let body_json = body.to_string();
                                                    let _ = preq.send_string(&body_json);
                                                }
                                            }
                                            // Clear active job and notify imported
                                            if let Ok(mut a) = active_job.lock() {
                                                *a = None;
                                            }
                                            let _ =
                                                tx_log.send(ModalEvent::JobImported(jid.clone()));
                                            break;
                                        }
                                    }
                                }
                            }
                            std::thread::sleep(Duration::from_millis(2000));
                        }
                    });
                }
                ModalEvent::CloudStatus { pending, running } => {
                    self.modal_queue_pending = pending;
                    self.modal_queue_running = running;
                    // Treat any queue activity as recent progress
                    if pending + running > 0 {
                        self.modal_last_progress_at = Some(Instant::now());
                    }
                }
                ModalEvent::CloudProgress {
                    job_id,
                    progress,
                    current,
                    total,
                    node_id,
                } => {
                    // Only track progress for the active job
                    let is_active = self
                        .modal_active_job
                        .lock()
                        .ok()
                        .and_then(|a| a.clone())
                        .map(|id| id == job_id)
                        .unwrap_or(true);
                    if !is_active {
                        continue;
                    }
                    self.modal_job_progress.insert(
                        job_id.clone(),
                        (progress, current, total, std::time::Instant::now()),
                    );
                    // Ensure an aggregate entry exists even if the queued id differed (e.g., prompt_id vs job_id)
                    let _ = self
                        .modal_phase_agg
                        .entry(job_id.clone())
                        .or_insert_with(PhaseAgg::default);
                    // Track as known job this session
                    self.modal_known_jobs.insert(job_id.clone());
                    if let Some(agg) = self.modal_phase_agg.get_mut(&job_id) {
                        // Map node to phase using plan
                        let phase = if let Some(nid) = node_id.as_ref() {
                            if self
                                .modal_phase_plans
                                .get(&job_id)
                                .map(|p| p.sampling.contains(nid))
                                .unwrap_or(false)
                            {
                                Some("s")
                            } else if self
                                .modal_phase_plans
                                .get(&job_id)
                                .map(|p| p.encoding.contains(nid))
                                .unwrap_or(false)
                            {
                                Some("e")
                            } else {
                                None
                            }
                        } else {
                            None
                        };
                        match phase {
                            Some("s") => {
                                agg.s_cur = agg.s_cur.max(current);
                                agg.s_tot = agg.s_tot.max(total);
                            }
                            Some("e") => {
                                agg.e_cur = agg.e_cur.max(current);
                                agg.e_tot = agg.e_tot.max(total);
                            }
                            _ => {
                                // Heuristic if node unknown
                                if total >= 32 {
                                    agg.e_cur = agg.e_cur.max(current);
                                    agg.e_tot = agg.e_tot.max(total);
                                } else {
                                    agg.s_cur = agg.s_cur.max(current);
                                    agg.s_tot = agg.s_tot.max(total);
                                }
                            }
                        }
                    }
                    self.modal_last_progress_at = Some(Instant::now());
                }
                ModalEvent::CloudSource { job_id, source } => {
                    // Only track source for the active job
                    let is_active = self
                        .modal_active_job
                        .lock()
                        .ok()
                        .and_then(|a| a.clone())
                        .map(|id| id == job_id)
                        .unwrap_or(true);
                    if is_active {
                        self.modal_job_source.insert(job_id, source);
                    }
                }
                ModalEvent::JobImporting(jid) => {
                    if let Some(a) = self.modal_phase_agg.get_mut(&jid) {
                        a.importing = true;
                    }
                }
                ModalEvent::JobImported(jid) => {
                    self.modal_phase_agg.remove(&jid);
                    self.modal_phase_plans.remove(&jid);
                    self.modal_job_progress.remove(&jid);
                    if let Ok(mut a) = self.modal_active_job.lock() {
                        if a.as_ref().map(|cur| cur == &jid).unwrap_or(false) {
                            *a = None;
                        }
                    }
                    self.modal_known_jobs.remove(&jid);
                    self.modal_monitor_requested = false;
                }
                ModalEvent::Recent(list) => {
                    self.modal_recent = list;
                }
            }
        }
        // Drain any completed files from ComfyUI ingest and import them
        while let Ok((proj_id, path)) = self.comfy_ingest_rx.try_recv() {
            // Determine project base path
            let mut base = self
                .db
                .get_project_base_path(&proj_id)
                .ok()
                .flatten()
                .unwrap_or_else(|| {
                    // Default base under app data dir if not set
                    let p = project::app_data_dir().join("projects").join(&proj_id);
                    let _ = std::fs::create_dir_all(&p);
                    let _ = self.db.set_project_base_path(&proj_id, &p);
                    p
                });
            // If base was incorrectly set to a file (e.g., from single-file import), use its parent dir.
            if base.is_file() {
                if let Some(parent) = base.parent() {
                    let parent = parent.to_path_buf();
                    let _ = self.db.set_project_base_path(&proj_id, &parent);
                    base = parent;
                }
            }
            let mut normalized_base = Self::normalize_project_base_path(&base);
            if normalized_base.as_os_str().is_empty() {
                normalized_base = base.clone();
            }
            if normalized_base != base {
                let _ = self.db.set_project_base_path(&proj_id, &normalized_base);
                base = normalized_base.clone();
            } else {
                base = normalized_base;
            }
            let media_dir = base.join("media").join("comfy");
            let date = chrono::Local::now().format("%Y-%m-%d").to_string();
            let dest_dir = media_dir.join(date);
            let _ = std::fs::create_dir_all(&dest_dir);
            let file_name = std::path::Path::new(&path)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "output.mp4".to_string());
            let mut dest = dest_dir.join(&file_name);
            // Ensure unique name
            if dest.exists() {
                let stem = dest
                    .file_stem()
                    .and_then(|s| Some(s.to_string_lossy().to_string()))
                    .unwrap_or_else(|| "output".to_string());
                let ext = dest
                    .extension()
                    .and_then(|e| Some(e.to_string_lossy().to_string()));
                let mut i = 1;
                loop {
                    let candidate = dest_dir.join(format!(
                        "{}-{}.{}",
                        stem,
                        i,
                        ext.as_deref().unwrap_or("mp4")
                    ));
                    if !candidate.exists() {
                        dest = candidate;
                        break;
                    }
                    i += 1;
                }
            }
            // True move semantics: try rename; on cross-device or other failures, copy then delete.
            let mut did_move = false;
            match std::fs::rename(&path, &dest) {
                Ok(_) => {
                    did_move = true;
                }
                Err(rename_err) => {
                    match std::fs::copy(&path, &dest) {
                        Ok(_) => {
                            // Best-effort remove original after successful copy
                            if let Err(rem_err) = std::fs::remove_file(&path) {
                                self.comfy_import_logs.push_back(format!(
                                    "Warning: copied (fallback) but failed to remove original {}: {}",
                                    path.to_string_lossy(), rem_err
                                ));
                            }
                        }
                        Err(copy_err) => {
                            self.comfy_import_logs.push_back(format!(
                                "Import move failed (rename: {}, copy: {}) {} -> {}",
                                rename_err,
                                copy_err,
                                path.to_string_lossy(),
                                dest.to_string_lossy(),
                            ));
                            continue; // Skip import on failure
                        }
                    }
                }
            }
            if let Some((_, metadata)) = self.apply_comfy_output_to_storyboard(ctx, &dest) {
                let mut map = std::collections::HashMap::new();
                map.insert(dest.clone(), metadata);
                let _ = self.import_files_for_with_metadata(&proj_id, &[dest.clone()], Some(map));
            } else {
                let _ = self.import_files_for(&proj_id, &[dest.clone()]);
            }
            let _ = self.db.set_project_base_path(&proj_id, &base);
            self.comfy_import_logs.push_back(if did_move {
                format!("Moved into {}: {}", proj_id, dest.to_string_lossy())
            } else {
                format!("Copied into {}: {}", proj_id, dest.to_string_lossy())
            });
        }
        self.process_comfy_events();
        // Start/stop ingest thread depending on state
        // Auto-import does not strictly require the server to be running;
        // as long as the ComfyUI repo/output folder is known, watch it.
        // If the open project changes, restart the watcher so events are
        // attributed to the project that was active when detection started.
        if let Some(pid) = &self.comfy_ingest_project_id {
            if Some(pid) != Some(&self.project_id) {
                if let Some(flag) = &self.comfy_ingest_stop {
                    flag.store(true, Ordering::Relaxed);
                }
                if let Some(h) = self.comfy_ingest_thread.take() {
                    let _ = h.join();
                }
                self.comfy_ingest_stop = None;
                self.comfy_ingest_project_id = None;
            }
        }
        let out_dir_cfg = self
            .comfy
            .config()
            .repo_path
            .as_ref()
            .map(|p| p.join("output"));
        let can_watch = out_dir_cfg.as_ref().map(|d| d.exists()).unwrap_or(false);
        if self.comfy_auto_import && can_watch {
            if self.comfy_ingest_thread.is_none() {
                if let Some(dir) = out_dir_cfg {
                    let dir_s = dir.to_string_lossy().to_string();
                    let stop = Arc::new(AtomicBool::new(false));
                    let tx = self.comfy_ingest_tx.clone();
                    let dir_clone = dir.clone();
                    let start_pid = self.project_id.clone();
                    let pid_for_thread = start_pid.clone();
                    let handle = std::thread::spawn({
                        let stop = Arc::clone(&stop);
                        move || {
                            use std::collections::{HashMap, HashSet};
                            use std::thread::sleep;
                            let mut seen: HashSet<String> = HashSet::new();
                            let mut stable: HashMap<String, (u64, u8)> = HashMap::new();
                            let allowed_exts = [
                                // videos
                                "mp4", "mov", "webm", "mkv", "avi", "gif", // images
                                "png", "jpg", "jpeg", "webp", "bmp", "tif", "tiff", "exr",
                            ];
                            while !stop.load(Ordering::Relaxed) {
                                seen.retain(|key| std::path::Path::new(key).exists());
                                stable.retain(|key, _| std::path::Path::new(key).exists());
                                for entry in
                                    WalkDir::new(&dir_clone).into_iter().filter_map(|e| e.ok())
                                {
                                    if !entry.file_type().is_file() {
                                        continue;
                                    }
                                    let p = entry.path();
                                    let ext = p
                                        .extension()
                                        .and_then(|e| e.to_str())
                                        .unwrap_or("")
                                        .to_ascii_lowercase();
                                    if !allowed_exts.contains(&ext.as_str()) {
                                        continue;
                                    }
                                    let key = p.to_string_lossy().to_string();
                                    if seen.contains(&key) {
                                        continue;
                                    }
                                    if let Ok(md) = entry.metadata() {
                                        let size = md.len();
                                        match stable.get_mut(&key) {
                                            Some((last, hits)) => {
                                                if *last == size {
                                                    *hits += 1;
                                                    if *hits >= 3 {
                                                        let _ = tx.send((
                                                            pid_for_thread.clone(),
                                                            p.to_path_buf(),
                                                        ));
                                                        stable.remove(&key);
                                                        seen.insert(key.clone());
                                                    }
                                                } else {
                                                    *last = size;
                                                    *hits = 0;
                                                }
                                            }
                                            None => {
                                                stable.insert(key.clone(), (size, 0));
                                            }
                                        }
                                    }
                                }
                                sleep(std::time::Duration::from_millis(700));
                            }
                        }
                    });
                    self.comfy_ingest_stop = Some(stop);
                    self.comfy_ingest_thread = Some(handle);
                    self.comfy_ingest_project_id = Some(start_pid);
                    self.comfy_import_logs
                        .push_back(format!("Watching Comfy outputs: {}", dir_s));
                }
            }
        } else {
            if let Some(flag) = &self.comfy_ingest_stop {
                flag.store(true, Ordering::Relaxed);
            }
            if let Some(h) = self.comfy_ingest_thread.take() {
                let _ = h.join();
            }
            self.comfy_ingest_stop = None;
            self.comfy_ingest_project_id = None;
        }

        // Cloud (Modal) websocket monitor removed; rely on HTTP polling.
        self.modal_monitor_requested = false;

        // Start/stop ComfyUI WebSocket job monitor
        // Runs regardless of embed; needs host/port and monitor toggle
        let ws_needed = self.comfy_ws_monitor;
        let ws_running = self.comfy_ws_thread.is_some();
        if ws_needed && !ws_running {
            let host = self.comfy.config().host.clone();
            let port = self.comfy.config().port;
            if !host.trim().is_empty() && port > 0 {
                let scheme_ws = if self.comfy.config().https {
                    "wss"
                } else {
                    "ws"
                };
                let encoded_client_id = urlencoding::encode(&self.comfy_client_id).into_owned();
                let ws_url = format!(
                    "{}://{}:{}/ws?clientId={}",
                    scheme_ws, host, port, encoded_client_id
                );
                let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let events_tx = self.comfy_ws_tx.clone();
                let auth_header = self.comfy_authorization_header();
                let handle = std::thread::spawn({
                    let stop = std::sync::Arc::clone(&stop);
                    let ws_url = ws_url.clone();
                    let auth_header = auth_header.clone();
                    move || {
                        use serde_json::Value;
                        use tungstenite::client::IntoClientRequest;
                        use tungstenite::Message;
                        let mut backoff_ms: u64 = 500;
                        loop {
                            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                                break;
                            }
                            let mut request = match ws_url.clone().into_client_request() {
                                Ok(req) => req,
                                Err(_) => break,
                            };
                            if let Some(ref auth) = auth_header {
                                if let Ok(value) = http::header::HeaderValue::from_str(auth) {
                                    request
                                        .headers_mut()
                                        .insert(http::header::AUTHORIZATION, value);
                                }
                            }
                            match tungstenite::client::connect(request) {
                                Ok((mut socket, _)) => {
                                    backoff_ms = 500;
                                    loop {
                                        if stop.load(std::sync::atomic::Ordering::Relaxed) {
                                            break;
                                        }
                                        match socket.read_message() {
                                            Ok(Message::Text(txt)) => {
                                                if let Ok(v) = serde_json::from_str::<Value>(&txt) {
                                                    if let Some(typ) =
                                                        v.get("type").and_then(|t| t.as_str())
                                                    {
                                                        match typ {
                                                            "status" => {
                                                                if let Some(status) = v
                                                                    .get("data")
                                                                    .and_then(|d| d.get("status"))
                                                                {
                                                                    let pending = status
                                                                        .get("queue_pending")
                                                                        .and_then(|arr| arr.as_array())
                                                                        .map(|arr| {
                                                                            arr.iter()
                                                                                .filter_map(|item| {
                                                                                    item.as_array().and_then(|a| a
                                                                                        .get(1)
                                                                                        .and_then(|v| v.as_str())
                                                                                        .map(|s| s.to_string()))
                                                                                })
                                                                                .collect::<Vec<String>>()
                                                                        })
                                                                        .unwrap_or_default();
                                                                    let running = status
                                                                        .get("queue_running")
                                                                        .and_then(|arr| arr.as_array())
                                                                        .map(|arr| {
                                                                            arr.iter()
                                                                                .filter_map(|item| {
                                                                                    item.as_array().and_then(|a| a
                                                                                        .get(1)
                                                                                        .and_then(|v| v.as_str())
                                                                                        .map(|s| s.to_string()))
                                                                                })
                                                                                .collect::<Vec<String>>()
                                                                        })
                                                                        .unwrap_or_default();
                                                                    let _ = events_tx.send(
                                                                        ComfyWsEvent::Queue {
                                                                            pending,
                                                                            running,
                                                                        },
                                                                    );
                                                                }
                                                            }
                                                            "progress" => {
                                                                if let Some(data) = v.get("data") {
                                                                    if let (
                                                                        Some(pid),
                                                                        Some(value),
                                                                        Some(max),
                                                                    ) = (
                                                                        data.get("prompt_id")
                                                                            .and_then(|s| {
                                                                                s.as_str()
                                                                            }),
                                                                        data.get("value").and_then(
                                                                            |n| n.as_f64(),
                                                                        ),
                                                                        data.get("max").and_then(
                                                                            |n| n.as_f64(),
                                                                        ),
                                                                    ) {
                                                                        let _ = events_tx.send(
                                                                            ComfyWsEvent::Progress {
                                                                                prompt_id:
                                                                                    pid.to_string(),
                                                                                value: value as f32,
                                                                                max: max as f32,
                                                                            },
                                                                        );
                                                                    }
                                                                }
                                                            }
                                                            "execution_start" => {
                                                                if let Some(data) = v.get("data") {
                                                                    if let Some(pid) = data
                                                                        .get("prompt_id")
                                                                        .and_then(|s| s.as_str())
                                                                    {
                                                                        let _ = events_tx.send(
                                                                            ComfyWsEvent::ExecutionStart {
                                                                                prompt_id:
                                                                                    pid.to_string(),
                                                                            },
                                                                        );
                                                                    }
                                                                }
                                                            }
                                                            "execution_end" => {
                                                                if let Some(data) = v.get("data") {
                                                                    if let Some(pid) = data
                                                                        .get("prompt_id")
                                                                        .and_then(|s| s.as_str())
                                                                    {
                                                                        let _ = events_tx.send(
                                                                            ComfyWsEvent::ExecutionEnd {
                                                                                prompt_id:
                                                                                    pid.to_string(),
                                                                            },
                                                                        );
                                                                    }
                                                                }
                                                            }
                                                            _ => {}
                                                        }
                                                    }
                                                }
                                            }
                                            Ok(Message::Binary(_)) => {}
                                            Ok(_) => {}
                                            Err(_) => {
                                                break;
                                            }
                                        }
                                    }
                                }
                                Err(_) => {
                                    std::thread::sleep(std::time::Duration::from_millis(
                                        backoff_ms,
                                    ));
                                    backoff_ms = (backoff_ms * 2).min(8_000);
                                }
                            }
                        }
                    }
                });
                self.comfy_ws_stop = Some(stop);
                self.comfy_ws_thread = Some(handle);
            }
        } else if !ws_needed && ws_running {
            if let Some(flag) = &self.comfy_ws_stop {
                flag.store(true, Ordering::Relaxed);
            }
            // Do not join here to avoid blocking UI if the socket is waiting; let it exit on its own.
            let _ = self.comfy_ws_thread.take();
            self.comfy_ws_stop = None;
        }
        if matches!(self.workspace_view, WorkspaceView::Timeline) {
            // Push-driven repaint is primary (worker triggers request_repaint on new frames).
            // Safety net: ensure periodic UI updates even if no frames arrive.
            if self.engine.state == PlayState::Playing {
                // Try to pace by the active clip fps, bounded by the sequence fps.
                let seq_fps = (self.seq.fps.num.max(1) as f64) / (self.seq.fps.den.max(1) as f64);
                let t_playhead = self.playback_clock.now();
                let active_fps =
                    if let Some((path, _)) = self.active_video_media_time_graph(t_playhead) {
                        if let Some(latest) = self.decode_mgr.take_latest(&path) {
                            latest.props.fps as f64
                        } else {
                            f64::NAN
                        }
                    } else {
                        f64::NAN
                    };
                let clip_fps = if active_fps.is_finite() && active_fps > 0.0 {
                    active_fps
                } else {
                    seq_fps
                };
                let target_fps = clip_fps.min(seq_fps).clamp(10.0, 120.0);
                let dt = 1.0f64 / target_fps;
                ctx.request_repaint_after(Duration::from_secs_f64(dt));
            } else {
                ctx.request_repaint_after(Duration::from_millis(150));
            }
            // Space toggles play/pause (keep engine.state in sync)
            if ctx.input(|i| i.key_pressed(egui::Key::Space)) {
                let seq_fps = (self.seq.fps.num.max(1) as f64) / (self.seq.fps.den.max(1) as f64);
                let current_sec = (self.playhead as f64) / seq_fps;

                if self.playback_clock.playing {
                    self.playback_clock.pause(current_sec);
                    self.engine.state = PlayState::Paused;
                    if let Some(engine) = &self.audio_out {
                        engine.pause(current_sec);
                    }
                } else {
                    if self.playhead >= self.seq.duration_in_frames {
                        self.playhead = 0;
                    }
                    self.playback_clock.play(current_sec);
                    self.engine.state = PlayState::Playing;
                    if let Ok(clips) = self.build_audio_clips() {
                        if let Some(engine) = &self.audio_out {
                            engine.start(current_sec, clips);
                        }
                    }
                }
            }
        } else {
            ctx.request_repaint_after(Duration::from_millis(200));
        }

        app_ui::top_toolbar(self, ctx, frame);

        if matches!(self.workspace_view, WorkspaceView::Timeline) {
            app_ui::preview_settings_window(self, ctx);
            app_screenplay::screenplay_window(self, ctx);
        }

        if app_ui::show_project_picker_if_needed(self, ctx) {
            return;
        }

        self.poll_jobs();

        if matches!(self.workspace_view, WorkspaceView::Timeline) {
            // Export dialog UI (editor mode only)
            self.export
                .ui(ctx, frame, &self.seq, &self.db, &self.project_id);

            // Preview panel will be inside CentralPanel with resizable area

            let assets = self.assets();
            let assets_panel = egui::SidePanel::left("assets")
                .default_width(200.0)
                .resizable(true)
                .min_width(110.0)
                .max_width(1600.0)
                .show(ctx, |ui| {
                    // Top area (not scrolling): toolbar + optional embedded ComfyUI
                    ui.heading("Assets");
                    ui.horizontal(|ui| {
                        if ui.button("Import...").clicked() {
                            if let Some(files) = self.file_dialog().pick_files() {
                                let _ = self.import_files(&files);
                            }
                        }
                        if ui.button("Refresh").clicked() {}
                        if ui.button("Jobs").clicked() {
                            self.show_jobs = !self.show_jobs;
                        }
                        if ui.button("ComfyUI").clicked() {
                            self.show_comfy_panel = !self.show_comfy_panel;
                        }
                    });
                    app_ui::proxy_jobs_summary(self, ui, &assets);
                    if self.show_comfy_panel {
                        app_ui::comfy_settings_panel(self, ui);
                    }
                    // Thumbnails are fixed-size squares to keep layout consistent
                    // Delegate full assets panel content to app_ui helpers
                    app_ui::cloud_modal_section(self, ui);
                    app_ui::comfy_embed_in_assets(self, ui);
                    app_ui::assets_scroll_section(self, ui, &assets);
                });

            self.assets_drop_rect = Some(assets_panel.response.rect);

            // Properties panel for selected clip
            app_ui::properties_panel(self, ctx);

            app_ui::center_editor(self, ctx, frame);

            app_ui::drag_overlay(self, ctx);

            self.handle_external_file_drops(ctx);
            self.process_pending_timeline_drops();
        } else {
            self.assets_drop_rect = None;
            self.timeline_drop_rect = None;
            match self.workspace_view {
                WorkspaceView::Chat => app_screenplay::chat_workspace(self, ctx),
                WorkspaceView::Storyboard => app_storyboard::storyboard_workspace(self, ctx),
                WorkspaceView::Timeline => {}
            }
        }

        self.jobs_window(ctx);
    }
}
