use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use anyhow::{anyhow, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use native_decoder::{create_decoder, DecoderConfig};
use once_cell::sync::Lazy;
use project::AssetRow;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// Global registry that tracks proxies on disk for each original asset.
static PROXY_REGISTRY: Lazy<RwLock<HashMap<PathBuf, PathBuf>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));
static DECODE_PROBE_CACHE: Lazy<RwLock<HashMap<PathBuf, bool>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProxyMode {
    OriginalOptimized,
    ProxyPreferred,
    ProxyOnly,
}

impl ProxyMode {
    pub fn display_name(self) -> &'static str {
        match self {
            ProxyMode::OriginalOptimized => "Original (Optimized)",
            ProxyMode::ProxyPreferred => "Proxy Preferred",
            ProxyMode::ProxyOnly => "Proxy Only",
        }
    }
}

impl Default for ProxyMode {
    fn default() -> Self {
        ProxyMode::OriginalOptimized
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackSource {
    Original,
    Proxy,
    Optimized,
}

#[derive(Debug, Clone)]
pub struct PlaybackSelection {
    pub path: String,
    pub source: PlaybackSource,
}

impl PlaybackSelection {
    fn from_path(path: &Path, source: PlaybackSource) -> Self {
        Self {
            path: path.to_string_lossy().into_owned(),
            source,
        }
    }
}

pub struct PlaybackSelector;

impl PlaybackSelector {
    pub fn select_path(
        asset: &AssetRow,
        mode: ProxyMode,
        optimized: Option<&Path>,
    ) -> Option<PlaybackSelection> {
        let mut optimized_info: Option<(PathBuf, bool)> =
            optimized.map(|p| (p.to_path_buf(), p.exists()));
        let mut optimized_skip_reason: Option<&'static str> = None;

        if let Some(opt_path) = optimized {
            let exists = opt_path.exists();
            optimized_info = Some((opt_path.to_path_buf(), exists));
            if exists {
                if ensure_decodable(opt_path) {
                    let selection =
                        PlaybackSelection::from_path(opt_path, PlaybackSource::Optimized);
                    let path_exists = Path::new(&selection.path).exists();
                    info!(
                        "[selector] asset={} chosen_tier=optimized path={} exists={}",
                        asset.id, selection.path, path_exists
                    );
                    return Some(selection);
                }
                optimized_skip_reason = Some("not_decodable");
                warn!(
                    optimized = %opt_path.display(),
                    original = %asset.src_abs,
                    "optimized media not decodable, ignoring"
                );
            }
        }

        let proxy_path = asset
            .proxy_path
            .as_ref()
            .map(PathBuf::from)
            .or_else(|| lookup_proxy(Path::new(&asset.src_abs)));

        let mut selection =
            PlaybackSelection::from_path(Path::new(&asset.src_abs), PlaybackSource::Original);

        match mode {
            ProxyMode::OriginalOptimized => {
                if asset.is_proxy_ready {
                    if let Some(proxy) = proxy_path.as_ref() {
                        if ensure_decodable(proxy.as_path()) {
                            selection = PlaybackSelection::from_path(
                                proxy.as_path(),
                                PlaybackSource::Proxy,
                            );
                        } else {
                            warn!(
                                proxy = %proxy.display(),
                                original = %asset.src_abs,
                                "proxy not decodable, falling back to original"
                            );
                        }
                    }
                }
            }
            ProxyMode::ProxyPreferred => {
                if let Some(proxy) = proxy_path.as_ref() {
                    if ensure_decodable(proxy.as_path()) {
                        selection =
                            PlaybackSelection::from_path(proxy.as_path(), PlaybackSource::Proxy);
                    } else {
                        warn!(
                            proxy = %proxy.display(),
                            original = %asset.src_abs,
                            "proxy not decodable, falling back to original"
                        );
                    }
                }
            }
            ProxyMode::ProxyOnly => {
                if let Some(proxy) = proxy_path.as_ref() {
                    if ensure_decodable(proxy.as_path()) {
                        selection =
                            PlaybackSelection::from_path(proxy.as_path(), PlaybackSource::Proxy);
                    } else {
                        warn!(
                            proxy = %proxy.display(),
                            "proxy not decodable, refusing to switch"
                        );
                    }
                } else {
                    warn!(
                        original = %asset.src_abs,
                        "proxy-only mode active but no usable proxy; falling back to original"
                    );
                }
            }
        }

        Some(selection).map(|selection| {
            let tier = match selection.source {
                PlaybackSource::Optimized => "optimized",
                PlaybackSource::Proxy => "proxy",
                PlaybackSource::Original => "original",
            };
            let path_exists = Path::new(&selection.path).exists();
            info!(
                "[selector] asset={} chosen_tier={} path={} exists={}",
                asset.id, tier, selection.path, path_exists
            );
            if let Some((_, true)) = optimized_info.as_ref() {
                if !matches!(selection.source, PlaybackSource::Optimized) {
                    let reason = optimized_skip_reason.unwrap_or("policy");
                    warn!(
                        "[warn] optimized path available but not used (reason: {})",
                        reason
                    );
                }
            }
            selection
        })
    }
}

/// Register or update the proxy mapping for an original media file.
pub fn register_proxy(original: PathBuf, proxy: PathBuf) {
    let orig_norm = canonicalize_for_map(&original);
    let proxy_norm = canonicalize_for_map(&proxy);
    let mut registry = PROXY_REGISTRY
        .write()
        .expect("proxy registry write lock poisoned");
    debug!(
        original = %orig_norm.display(),
        proxy = %proxy_norm.display(),
        "registering proxy mapping"
    );
    registry.insert(orig_norm.clone(), proxy_norm.clone());

    if let Ok(mut cache) = DECODE_PROBE_CACHE.write() {
        cache.remove(&proxy_norm);
    }
}

/// Look up the proxy path for a given original source, if one exists.
pub fn lookup_proxy(original: &Path) -> Option<PathBuf> {
    let registry = PROXY_REGISTRY
        .read()
        .expect("proxy registry read lock poisoned");
    registry.get(&canonicalize_for_map(original)).cloned()
}

fn canonicalize_for_map(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn ensure_decodable(path: &Path) -> bool {
    let key = canonicalize_for_map(path);
    {
        let cache = DECODE_PROBE_CACHE
            .read()
            .expect("decode probe cache read lock poisoned");
        if let Some(result) = cache.get(&key) {
            return *result;
        }
    }

    let probe_result = match create_decoder(&key, DecoderConfig::default()) {
        Ok(mut decoder) => {
            // Touch properties to ensure initialization before dropping.
            let _ = decoder.get_properties();
            true
        }
        Err(err) => {
            warn!(
                candidate = %path.display(),
                error = %err,
                "failed to initialize decoder for playback candidate"
            );
            false
        }
    };

    let mut cache = DECODE_PROBE_CACHE
        .write()
        .expect("decode probe cache write lock poisoned");
    cache.insert(key, probe_result);
    probe_result
}

/// Build a GStreamer pipeline that plays either the original asset or its proxy.
///
/// Returns the configured pipeline so that the caller can manage its lifetime.
pub fn play_media(media_path: &str, use_proxies: bool) -> Result<gst::Pipeline> {
    gst::init().map_err(|err| anyhow!("failed to init GStreamer: {err}"))?;

    let requested_path = PathBuf::from(media_path);
    let selected_path = if use_proxies {
        lookup_proxy(&requested_path)
            .map(|proxy| {
                info!(
                    original = %requested_path.display(),
                    proxy = %proxy.display(),
                    "using proxy for playback"
                );
                proxy
            })
            .unwrap_or_else(|| {
                warn!(
                    original = %requested_path.display(),
                    "no proxy registered, falling back to original media"
                );
                requested_path.clone()
            })
    } else {
        requested_path.clone()
    };

    let pipeline = gst::Pipeline::new();
    let filesrc = gst::ElementFactory::make("filesrc")
        .property("location", &selected_path.to_string_lossy().to_string())
        .build()
        .context("construct filesrc element")?;
    let decodebin = gst::ElementFactory::make("decodebin")
        .build()
        .context("construct decodebin element")?;
    let videosink = gst::ElementFactory::make("autovideosink")
        .build()
        .context("construct autovideosink element")?;

    pipeline
        .add_many(&[&filesrc, &decodebin, &videosink])
        .context("add playback elements to pipeline")?;
    gst::Element::link(&filesrc, &decodebin).context("link filesrc to decodebin")?;

    let sink_weak = videosink.downgrade();
    decodebin.connect_pad_added(move |_dbin, src_pad| {
        let Some(caps) = src_pad
            .current_caps()
            .or_else(|| Some(src_pad.query_caps(None)))
        else {
            return;
        };
        let Some(structure) = caps.structure(0) else {
            return;
        };
        if !structure.name().starts_with("video/") {
            return;
        }

        if let Some(sink) = sink_weak.upgrade() {
            let sink_pad = sink.static_pad("sink").expect("videosink lacks sink pad");
            if sink_pad.is_linked() {
                return;
            }

            if let Err(link_err) = src_pad.link(&sink_pad) {
                warn!(error = %link_err, "failed to link decodebin to videosink");
            }
        }
    });

    pipeline
        .set_state(gst::State::Playing)
        .context("set playback pipeline to Playing state")?;

    Ok(pipeline)
}

/// Example UI usage:
///
/// ```ignore
/// // Somewhere in egui code:
/// if ui.button("Toggle Proxy Playback").clicked() {
///     app_state.use_proxies = !app_state.use_proxies;
/// }
/// // Later, when the user plays a clip:
/// let _pipeline = playback_selector::play_media(&clip.original_path, app_state.use_proxies)?;
/// ```
///
/// The button simply flips a boolean in whatever application state container you use.
/// That flag is then passed into [`play_media`] whenever playback is initiated.
pub fn example_ui_toggle_explanation() {}
