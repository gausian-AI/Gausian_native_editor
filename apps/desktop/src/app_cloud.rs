use crossbeam_channel::Sender;
use serde_json::Value as JsonValue;
use std::collections::HashMap;

use super::App;

// Cloud/Modal helpers extracted from App
pub(super) fn modal_test_connection(app: &App) {
    let base = app.modal_base_url.trim().to_string();
    let key = app.modal_api_key.clone();
    let tx = app.modal_tx.clone();
    std::thread::spawn(move || {
        let log = |s: &str| {
            let _ = tx.send(super::ModalEvent::Log(s.to_string()));
        };
        if base.is_empty() {
            log("Base URL not set");
            return;
        }
        // Normalize base (strip trailing /health or /healthz if user pasted a full health URL)
        let mut base_trim = base.trim_end_matches('/').to_string();
        for suffix in ["/healthz", "/health"] {
            if base_trim.ends_with(suffix) {
                base_trim = base_trim[..base_trim.len() - suffix.len()]
                    .trim_end_matches('/')
                    .to_string();
                break;
            }
        }
        // Try extended health first (/healthz) to list recent artifacts; fall back to /health
        let base_trim = base_trim; // shadow immutable
        let urlz = format!("{}/healthz", base_trim);
        let mut reqz = ureq::get(&urlz);
        if !key.trim().is_empty() {
            reqz = reqz.set("Authorization", &format!("Bearer {}", key));
        }
        match reqz.call() {
            Ok(resp) => {
                let status = resp.status();
                match resp.into_string() {
                    Ok(body) => {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
                            let recent = v
                                .get("recent")
                                .and_then(|r| r.as_array())
                                .map(|a| a.len())
                                .unwrap_or(0);
                            log(&format!("Healthz: {} (recent jobs: {})", status, recent));
                            if let Some(arr) = v.get("recent").and_then(|r| r.as_array()) {
                                for (i, j) in arr.iter().enumerate().take(3) {
                                    let jid = j.get("id").and_then(|s| s.as_str()).unwrap_or("");
                                    let st = j.get("status").and_then(|s| s.as_str()).unwrap_or("");
                                    if let Some(arts) =
                                        j.get("artifacts").and_then(|a| a.as_array())
                                    {
                                        for (k, a) in arts.iter().enumerate().take(2) {
                                            let fname = a
                                                .get("filename")
                                                .and_then(|s| s.as_str())
                                                .unwrap_or("");
                                            let url =
                                                a.get("url").and_then(|s| s.as_str()).unwrap_or("");
                                            log(&format!(
                                                "  [{}] {} {} -> {}",
                                                i + 1,
                                                jid,
                                                fname,
                                                url
                                            ));
                                            if k == 0 {
                                                break;
                                            }
                                        }
                                    } else {
                                        log(&format!("  [{}] {} (status: {})", i + 1, jid, st));
                                    }
                                }
                            }
                            return;
                        } else {
                            log(&format!("Healthz: {} (non-JSON)", status));
                            return;
                        }
                    }
                    Err(_) => {
                        log(&format!("Healthz: {} (empty body)", status));
                        return;
                    }
                }
            }
            Err(_e) => {
                let url = format!("{}/health", base_trim);
                let req = ureq::get(&url);
                let req = if key.trim().is_empty() {
                    req
                } else {
                    req.set("Authorization", &format!("Bearer {}", key))
                };
                match req.call() {
                    Ok(resp) => log(&format!("Health: {}", resp.status())),
                    Err(e) => log(&format!("Health check failed: {}", e)),
                }
            }
        }
    });
}

pub(super) fn modal_queue_job(app: &App) {
    let base = app.modal_base_url.trim().to_string();
    let key = app.modal_api_key.clone();
    let payload = app.modal_payload.clone();
    let tx = app.modal_tx.clone();
    let target = app.cloud_target;
    // Generate a unique client_id to tag this job's outputs (prefix)
    // Also used by ComfyUI WS to stream progress for this job.
    let client_id = uuid::Uuid::new_v4().to_string();
    let short_id: String = client_id.chars().take(8).collect();
    std::thread::spawn(move || {
        let log = |s: &str| {
            let _ = tx.send(super::ModalEvent::Log(s.to_string()));
        };
        if base.is_empty() {
            log("Base URL not set");
            return;
        }
        if payload.trim().is_empty() {
            log("Payload is empty");
            return;
        }
        let url = format!("{}/prompt", base.trim_end_matches('/'));
        let req_base = ureq::post(&url).set("Content-Type", "application/json");
        let req_base = if key.trim().is_empty() {
            req_base
        } else {
            req_base.set("Authorization", &format!("Bearer {}", key))
        };
        // Prepare body depending on target, and patch filename_prefix/client_id for unique outputs
        let mut body_v: serde_json::Value = match target {
            super::CloudTarget::Prompt => {
                // If payload already has {"prompt":{...}}, patch it; else wrap it
                match serde_json::from_str::<serde_json::Value>(&payload) {
                    Ok(mut v) => {
                        if v.get("prompt").is_some() {
                            v
                        } else {
                            let mut obj = serde_json::Map::new();
                            obj.insert("prompt".into(), v);
                            serde_json::Value::Object(obj)
                        }
                    }
                    Err(e) => {
                        log(&format!("Invalid JSON: {}", e));
                        return;
                    }
                }
            }
            super::CloudTarget::Workflow => match convert_workflow_to_prompt(&payload) {
                Ok(s) => match serde_json::from_str::<serde_json::Value>(&s) {
                    Ok(v) => v,
                    Err(e) => {
                        log(&format!("Converted workflow parse failed: {}", e));
                        return;
                    }
                },
                Err(e) => {
                    log(&format!("Workflow convert failed: {}", e));
                    return;
                }
            },
        };
        // Ensure client_id/filename_prefix presence where possible
        if let Some(prompt_obj) = body_v.get_mut("prompt").and_then(|p| p.as_object_mut()) {
            for node_v in prompt_obj.values_mut() {
                if let Some(nobj) = node_v.as_object_mut() {
                    // Provide inputs map
                    let inputs = nobj
                        .entry("inputs")
                        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
                    if let Some(inputs_obj) = inputs.as_object_mut() {
                        let mut prefix_val = inputs_obj
                            .get("filename_prefix")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        if prefix_val.is_empty() {
                            // If empty, default to short job-scoped id
                            inputs_obj.insert(
                                "filename_prefix".into(),
                                serde_json::Value::String(format!("{}", short_id)),
                            );
                        } else {
                            // If present, append job-scoped suffix when missing
                            if !prefix_val.contains(&short_id) {
                                prefix_val = format!("{}-{}", prefix_val, short_id);
                                inputs_obj.insert(
                                    "filename_prefix".into(),
                                    serde_json::Value::String(prefix_val),
                                );
                            }
                        }
                    }
                }
            }
        }
        // Ensure top-level client_id is present so the backend and ComfyUI agree
        if !body_v
            .get("client_id")
            .and_then(|v| v.as_str())
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
        {
            if let serde_json::Value::Object(ref mut obj) = body_v {
                obj.insert(
                    "client_id".into(),
                    serde_json::Value::String(client_id.clone()),
                );
            }
        }
        let body = body_v.to_string();
        match req_base.send_string(&body) {
            Ok(resp) => {
                let status = resp.status();
                if status >= 200 && status < 300 {
                    match resp.into_string() {
                        Ok(body) => {
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
                                let id = v
                                    .get("prompt_id")
                                    .or_else(|| v.get("job_id"))
                                    .or_else(|| v.get("id"))
                                    .and_then(|s| s.as_str())
                                    .unwrap_or("");
                                if !id.is_empty() {
                                    let _ = tx.send(super::ModalEvent::JobQueued(id.to_string()));
                                    // Also include the unique prefix used for this run (preserved base + client_id)
                                    // Try to extract what we actually set for filename_prefix
                                    let mut prefix_used = client_id.clone();
                                    if let Some(prompt_obj) =
                                        body_v.get("prompt").and_then(|p| p.as_object())
                                    {
                                        for node_v in prompt_obj.values() {
                                            if let Some(nobj) = node_v.as_object() {
                                                if let Some(inputs) =
                                                    nobj.get("inputs").and_then(|i| i.as_object())
                                                {
                                                    if let Some(fpv) = inputs
                                                        .get("filename_prefix")
                                                        .and_then(|x| x.as_str())
                                                    {
                                                        if !fpv.trim().is_empty() {
                                                            prefix_used = fpv.trim().to_string();
                                                            break;
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    let _ = tx.send(super::ModalEvent::JobQueuedWithPrefix(
                                        id.to_string(),
                                        prefix_used,
                                    ));
                                } else {
                                    log("Job queued (no id in response)");
                                }
                            } else {
                                log("Job queued (non-JSON response)");
                            }
                        }
                        Err(_) => {
                            log("Job queued (no body)");
                        }
                    }
                } else {
                    let body = resp.into_string().unwrap_or_default();
                    log(&format!("Queue failed: HTTP {}\n{}", status, body));
                }
            }
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                log(&format!("Queue failed: HTTP {}\n{}", code, body));
            }
            Err(e) => log(&format!("Queue error: {}", e)),
        }
    });
}

// Thin App method wrappers to keep app.rs small
impl App {
    pub(crate) fn modal_test_connection(&self) {
        self::modal_test_connection(self)
    }

    pub(crate) fn modal_queue_job(&self) {
        self::modal_queue_job(self)
    }

    pub(crate) fn compute_phase_plan_from_payload(payload: &str) -> super::PhasePlan {
        self::compute_phase_plan_from_payload(payload)
    }

    pub(crate) fn modal_refresh_recent(&self) {
        self::modal_refresh_recent(self)
    }

    pub(crate) fn modal_import_url(&self, url: String, suggested_name: Option<String>) {
        self::modal_import_url(self, url, suggested_name)
    }
}

pub(super) fn compute_phase_plan_from_payload(payload: &str) -> super::PhasePlan {
    let mut plan = super::PhasePlan::default();
    let parse = serde_json::from_str::<serde_json::Value>(payload).ok();
    let mut prompt_obj_opt: Option<&serde_json::Map<String, serde_json::Value>> = None;
    if let Some(v) = parse.as_ref() {
        if let Some(p) = v.get("prompt").and_then(|p| p.as_object()) {
            prompt_obj_opt = Some(p);
        } else if v.get("nodes").is_some() {
            // Workflow format; build a temporary prompt-like map
            if let Some(arr) = v.get("nodes").and_then(|n| n.as_array()) {
                let mut tmp = serde_json::Map::new();
                for n in arr {
                    if let (Some(id), Some(ct)) =
                        (n.get("id"), n.get("class_type").and_then(|s| s.as_str()))
                    {
                        let id_s = if let Some(i) = id.as_i64() {
                            i.to_string()
                        } else {
                            id.as_str().unwrap_or("").to_string()
                        };
                        let mut o = serde_json::Map::new();
                        o.insert(
                            "class_type".into(),
                            serde_json::Value::String(ct.to_string()),
                        );
                        tmp.insert(id_s, serde_json::Value::Object(o));
                    }
                }
                prompt_obj_opt = Some(&*Box::leak(Box::new(tmp))); // limited scope in UI; acceptable here
            }
        } else if v.is_object() {
            prompt_obj_opt = v.as_object();
        }
    }
    if let Some(prompt_obj) = prompt_obj_opt {
        for (id, nodev) in prompt_obj {
            if let Some(ct) = nodev.get("class_type").and_then(|s| s.as_str()) {
                let id_s = id.clone();
                let lc = ct.to_ascii_lowercase();
                if lc.contains("ksampler") || lc.contains("modelsampling") {
                    plan.sampling.insert(id_s);
                }
                if matches!(ct, "VHS_VideoCombine" | "VideoCombine" | "SaveVideo")
                    || lc.contains("videocombine")
                    || lc.contains("savevideo")
                {
                    plan.encoding.insert(id.clone());
                }
            }
        }
    }
    plan
}

pub(super) fn modal_refresh_recent(app: &App) {
    let base = app.modal_base_url.trim().to_string();
    let key = app.modal_api_key.clone();
    let tx = app.modal_tx.clone();
    std::thread::spawn(move || {
        let log = |s: &str| {
            let _ = tx.send(super::ModalEvent::Log(s.to_string()));
        };
        if base.is_empty() {
            log("Base URL not set");
            return;
        }
        // Normalize base
        let mut base_trim = base.trim_end_matches('/').to_string();
        for suffix in ["/healthz", "/health"] {
            if base_trim.ends_with(suffix) {
                base_trim = base_trim[..base_trim.len() - suffix.len()]
                    .trim_end_matches('/')
                    .to_string();
                break;
            }
        }
        let url = format!("{}/healthz", base_trim);
        let mut req = ureq::get(&url);
        if !key.trim().is_empty() {
            req = req.set("Authorization", &format!("Bearer {}", key));
        }
        match req.call() {
            Ok(resp) => match resp.into_string() {
                Ok(body) => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
                        let mut list: Vec<(String, Vec<(String, String)>)> = Vec::new();
                        if let Some(arr) = v.get("recent").and_then(|r| r.as_array()) {
                            for j in arr.iter() {
                                let jid = j
                                    .get("id")
                                    .and_then(|s| s.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let mut arts: Vec<(String, String)> = Vec::new();
                                if let Some(a) = j.get("artifacts").and_then(|a| a.as_array()) {
                                    for it in a.iter() {
                                        let fname = it
                                            .get("filename")
                                            .and_then(|s| s.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        let url = it
                                            .get("url")
                                            .and_then(|s| s.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        if !url.is_empty() {
                                            arts.push((fname, url));
                                        }
                                    }
                                }
                                if !jid.is_empty() {
                                    list.push((jid, arts));
                                }
                            }
                        }
                        let _ = tx.send(super::ModalEvent::Recent(list));
                    } else {
                        log("/healthz returned non-JSON");
                    }
                }
                Err(e) => {
                    log(&format!("/healthz read error: {}", e));
                }
            },
            Err(e) => {
                log(&format!("/healthz failed: {}", e));
                // Fallback to /health to at least verify connectivity
                let url = format!("{}/health", base_trim);
                let mut req = ureq::get(&url);
                if !key.trim().is_empty() {
                    req = req.set("Authorization", &format!("Bearer {}", key));
                }
                let _ = req.call().ok();
                let _ = tx.send(super::ModalEvent::Recent(Vec::new()));
            }
        }
    });
}

pub(super) fn modal_import_url(app: &App, url: String, suggested_name: Option<String>) {
    let token = app.modal_api_key.clone();
    let tx_import = app.comfy_ingest_tx.clone();
    let proj_id = app.project_id.clone();
    let tx_log = app.modal_tx.clone();
    std::thread::spawn(move || {
        let log = |s: &str| {
            let _ = tx_log.send(super::ModalEvent::Log(s.to_string()));
        };
        let mut req = ureq::get(&url);
        if !token.trim().is_empty() {
            req = req.set("Authorization", &format!("Bearer {}", token));
        }
        match req.call() {
            Ok(resp) => {
                let fname = suggested_name
                    .clone()
                    .filter(|s| !s.is_empty())
                    .or_else(|| {
                        // derive from URL path
                        url::Url::parse(&url).ok().and_then(|u| {
                            u.path_segments()
                                .and_then(|mut p| p.next_back())
                                .map(|s| s.to_string())
                        })
                    })
                    .unwrap_or_else(|| "artifact.mp4".to_string());
                let tmpdir = project::app_data_dir().join("tmp").join("cloud");
                let _ = std::fs::create_dir_all(&tmpdir);
                let tmp = tmpdir.join(fname);
                match std::fs::File::create(&tmp) {
                    Ok(mut f) => {
                        let mut reader = resp.into_reader();
                        if let Err(e) = std::io::copy(&mut reader, &mut f) {
                            log(&format!("Download write failed: {}", e));
                            return;
                        }
                        let _ = tx_import.send((proj_id.clone(), tmp.clone()));
                        log(&format!(
                            "Downloaded → queued import: {}",
                            tmp.to_string_lossy()
                        ));
                    }
                    Err(e) => {
                        log(&format!("Temp create failed: {}", e));
                    }
                }
            }
            Err(e) => log(&format!("Download failed: {}", e)),
        }
    });
}

// Best-effort converter from a generic "workflow" JSON into a ComfyUI /prompt payload.
// This is intentionally conservative: it tries to recognize a "nodes" array and
// build a minimal prompt map with class_type and any provided literal inputs.
// Complex graph links are not guaranteed to convert; if conversion isn't possible,
// returns an Err with a helpful message.
fn is_ui_only_node(
    class_type: Option<&str>,
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> bool {
    if let Some(class) = class_type.map(str::trim) {
        let lower = class.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "markdownnote" | "markdown_note" | "markdown note" | "note"
        ) {
            return true;
        }
    }
    if let Some(meta) = meta {
        if let Some(category) = meta.get("category").and_then(|s| s.as_str()) {
            if category.eq_ignore_ascii_case("ui") {
                return true;
            }
        }
        if meta
            .get("ui_only")
            .and_then(|flag| flag.as_bool())
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

pub(super) fn strip_ui_only_prompt_nodes(prompt: &mut serde_json::Map<String, serde_json::Value>) {
    let to_remove: Vec<String> = prompt
        .iter()
        .filter_map(|(id, node)| {
            let obj = node.as_object()?;
            if is_ui_only_node(
                obj.get("class_type").and_then(|v| v.as_str()),
                obj.get("_meta").and_then(|m| m.as_object()),
            ) {
                Some(id.clone())
            } else {
                None
            }
        })
        .collect();
    for key in to_remove {
        prompt.remove(&key);
    }
}

fn widget_value(values: &[serde_json::Value], idx: usize) -> Option<serde_json::Value> {
    values.get(idx).cloned().filter(|v| !v.is_null())
}

fn node_widget_inputs(
    class_type: &str,
    widgets: &serde_json::Value,
) -> Vec<(String, serde_json::Value)> {
    let mut pairs = Vec::new();
    if let Some(map) = widgets.as_object() {
        for (key, value) in map {
            if value.is_null() || value.is_object() {
                continue;
            }
            pairs.push((key.clone(), value.clone()));
        }
        return pairs;
    }
    let Some(values) = widgets.as_array() else {
        return pairs;
    };
    match class_type {
        "SaveVideo" => {
            if let Some(v) = widget_value(values, 0) {
                pairs.push(("filename_prefix".into(), v));
            }
            if let Some(v) = widget_value(values, 1) {
                pairs.push(("format".into(), v));
            }
            if let Some(v) = widget_value(values, 2) {
                pairs.push(("codec".into(), v));
            }
        }
        "SaveImage" | "ImageSave" | "SaveImageBuiltin" => {
            if let Some(v) = widget_value(values, 0) {
                pairs.push(("filename_prefix".into(), v));
            }
        }
        "CreateVideo" => {
            if let Some(v) = widget_value(values, 0) {
                pairs.push(("fps".into(), v));
            }
        }
        "WanImageToVideo" => {
            if let Some(v) = widget_value(values, 0) {
                pairs.push(("width".into(), v));
            }
            if let Some(v) = widget_value(values, 1) {
                pairs.push(("height".into(), v));
            }
            if let Some(v) = widget_value(values, 2) {
                pairs.push(("length".into(), v));
            }
            if let Some(v) = widget_value(values, 3) {
                pairs.push(("batch_size".into(), v));
            }
        }
        "Wan22ImageToVideoLatent" => {
            if let Some(v) = widget_value(values, 0) {
                pairs.push(("width".into(), v));
            }
            if let Some(v) = widget_value(values, 1) {
                pairs.push(("height".into(), v));
            }
            if let Some(v) = widget_value(values, 2) {
                pairs.push(("length".into(), v));
            }
            if let Some(v) = widget_value(values, 3) {
                pairs.push(("batch_size".into(), v));
            }
        }
        "ModelSamplingSD3" => {
            if let Some(v) = widget_value(values, 0) {
                pairs.push(("shift".into(), v));
            }
        }
        "UNETLoader" => {
            if let Some(v) = widget_value(values, 0) {
                pairs.push(("unet_name".into(), v));
            }
            if let Some(v) = widget_value(values, 1) {
                pairs.push(("weight_dtype".into(), v));
            }
        }
        "LoraLoaderModelOnly" => {
            if let Some(v) = widget_value(values, 0) {
                pairs.push(("lora_name".into(), v));
            }
            if let Some(v) = widget_value(values, 1) {
                pairs.push(("strength_model".into(), v));
            }
        }
        "CLIPLoader" => {
            if let Some(v) = widget_value(values, 0) {
                pairs.push(("clip_name".into(), v));
            }
            if let Some(v) = widget_value(values, 1) {
                pairs.push(("type".into(), v));
            }
            if let Some(v) = widget_value(values, 2) {
                pairs.push(("device".into(), v));
            }
        }
        "CLIPTextEncode" => {
            if let Some(v) = widget_value(values, 0) {
                pairs.push(("text".into(), v));
            }
            if let Some(v) = widget_value(values, 1) {
                pairs.push(("clip_skip".into(), v));
            }
        }
        "VAELoader" => {
            if let Some(v) = widget_value(values, 0) {
                pairs.push(("vae_name".into(), v));
            }
        }
        "LoadImage" => {
            if let Some(v) = widget_value(values, 0) {
                pairs.push(("image".into(), v));
            }
        }
        "KSamplerAdvanced" => {
            let keys = [
                "add_noise",
                "noise_seed",
                "noise_seed_behavior",
                "steps",
                "cfg",
                "sampler_name",
                "scheduler",
                "start_at_step",
                "end_at_step",
                "return_with_leftover_noise",
            ];
            for (idx, key) in keys.iter().enumerate() {
                if let Some(v) = widget_value(values, idx) {
                    pairs.push((key.to_string(), v));
                }
            }
        }
        "KSampler" => {
            let keys = [
                "seed",
                "steps",
                "cfg",
                "sampler_name",
                "scheduler",
                "denoise",
            ];
            for (idx, key) in keys.iter().enumerate() {
                if let Some(v) = widget_value(values, idx) {
                    pairs.push((key.to_string(), v));
                }
            }
        }
        _ => {}
    }
    pairs
}

pub(super) fn convert_workflow_to_prompt(workflow_json: &str) -> Result<String, String> {
    let mut v: serde_json::Value =
        serde_json::from_str(workflow_json).map_err(|e| format!("Invalid JSON: {}", e))?;
    if let Some(prompt) = v.get_mut("prompt").and_then(|p| p.as_object_mut()) {
        strip_ui_only_prompt_nodes(prompt);
        return Ok(v.to_string());
    }
    // If it's already a node-id keyed object with class_type, wrap as prompt
    if let Some(obj) = v.as_object() {
        let looks_like_prompt = obj.values().all(|n| n.get("class_type").is_some());
        if looks_like_prompt {
            let mut prompt = obj.clone();
            strip_ui_only_prompt_nodes(&mut prompt);
            let mut wrap = serde_json::Map::new();
            wrap.insert("prompt".into(), serde_json::Value::Object(prompt));
            wrap.insert(
                "client_id".into(),
                serde_json::Value::String(uuid::Uuid::new_v4().to_string()),
            );
            return Ok(serde_json::Value::Object(wrap).to_string());
        }
    }
    // Try workflow format with nodes[]
    let nodes = v.get("nodes").and_then(|n| n.as_array()).ok_or_else(|| {
        "Workflow JSON doesn't contain a 'prompt' or a 'nodes' array; please paste a ComfyUI API prompt (Copy API)".to_string()
    })?;
    let mut link_map: HashMap<i64, (String, i64)> = HashMap::new();
    if let Some(links) = v.get("links").and_then(|l| l.as_array()) {
        for link in links {
            if let Some(arr) = link.as_array() {
                if let (Some(id), Some(src_node), Some(src_slot)) = (
                    arr.get(0).and_then(|v| v.as_i64()),
                    arr.get(1).and_then(|v| v.as_i64()),
                    arr.get(2).and_then(|v| v.as_i64()),
                ) {
                    link_map.insert(id, (src_node.to_string(), src_slot));
                }
            }
        }
    }
    let mut prompt = serde_json::Map::new();
    for node in nodes {
        let id_val = node
            .get("id")
            .ok_or_else(|| "Node missing 'id'".to_string())?;
        let id_str = if let Some(n) = id_val.as_i64() {
            n.to_string()
        } else {
            id_val.as_str().unwrap_or("").to_string()
        };
        if id_str.is_empty() {
            return Err("Node id is empty".into());
        }
        let class_type = node
            .get("class_type")
            .or_else(|| node.get("type"))
            .or_else(|| node.get("class"))
            .and_then(|s| s.as_str())
            .ok_or_else(|| format!("Node {} missing class_type", id_str))?;
        if is_ui_only_node(
            Some(class_type),
            node.get("_meta").and_then(|m| m.as_object()),
        ) {
            continue;
        }
        let mut inputs_map = serde_json::Map::new();
        if let Some(inputs_arr) = node.get("inputs").and_then(|i| i.as_array()) {
            for input in inputs_arr {
                if let Some(name) = input.get("name").and_then(|n| n.as_str()) {
                    if let Some(link_id) = input.get("link").and_then(|l| l.as_i64()) {
                        if let Some((src_id, src_slot)) = link_map.get(&link_id) {
                            inputs_map.insert(
                                name.to_string(),
                                serde_json::Value::Array(vec![
                                    serde_json::Value::String(src_id.clone()),
                                    serde_json::Value::Number(serde_json::Number::from(*src_slot)),
                                ]),
                            );
                        }
                    } else if let Some(value) = input.get("value").cloned() {
                        inputs_map.insert(name.to_string(), value);
                    }
                }
            }
        }
        if let Some(widgets) = node.get("widgets_values") {
            for (key, value) in node_widget_inputs(class_type, widgets) {
                inputs_map.entry(key).or_insert(value);
            }
        }
        let mut nobj = serde_json::Map::new();
        nobj.insert(
            "class_type".into(),
            serde_json::Value::String(class_type.to_string()),
        );
        nobj.insert("inputs".into(), serde_json::Value::Object(inputs_map));
        let mut meta_map = node
            .get("_meta")
            .and_then(|m| m.as_object())
            .cloned()
            .unwrap_or_else(serde_json::Map::new);
        let title_from_meta = meta_map
            .get("title")
            .and_then(|v| v.as_str())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let title_from_node = node
            .get("title")
            .and_then(|v| v.as_str())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        if let Some(title) = title_from_node.clone() {
            meta_map.insert("title".into(), serde_json::Value::String(title.clone()));
        }
        let effective_title = title_from_node.or(title_from_meta);
        if let Some(title) = effective_title {
            nobj.insert("title".into(), serde_json::Value::String(title));
        }
        if !meta_map.is_empty() {
            nobj.insert("_meta".into(), serde_json::Value::Object(meta_map));
        }
        prompt.insert(id_str, serde_json::Value::Object(nobj));
    }
    strip_ui_only_prompt_nodes(&mut prompt);
    let mut root = serde_json::Map::new();
    root.insert("prompt".into(), serde_json::Value::Object(prompt));
    root.insert(
        "client_id".into(),
        serde_json::Value::String(uuid::Uuid::new_v4().to_string()),
    );
    Ok(serde_json::Value::Object(root).to_string())
}
