# Gausian Native Editor

A fast, native video editor and preview tool written in Rust with optional cloud rendering/encoding via Modal and ComfyUI.

This README captures purpose/scope, current architecture, exact stack versions, how to run, and recent decisions so the project state stays discoverable.

## Purpose / Scope

- Native desktop editor (Rust/egui) with timeline, assets, and real‑time preview
- Local media import (FFmpeg/ffprobe), auto‑import from ComfyUI outputs
- Cloud workflow (optional): submit ComfyUI jobs and monitor completions; import finished artifacts automatically
- Modal scaffold (optional): H100 generator (frames) + L4 encoder (NVENC)

## Architecture (brief)

- apps/desktop (Rust/egui):
  - Assets panel, timeline, GPU preview, export
  - Local ComfyUI integration (embed optional; default OFF)
  - Auto‑import from ComfyUI output folder (videos + images; true move semantics)
  - Cloud section: queue job to /prompt, live job monitors (ComfyUI WS and Cloud WS)

- crates/* (Rust):
  - timeline: graph, tracks, clips, command history
  - project: SQLite DB, assets table, project timeline JSON persistence
  - media-io: FFmpeg probing/exports (requires ffprobe on PATH)
  - exporters, renderer, jobs, plugin-host (as in source tree)

- modal_app (Python; scaffold):
  - generate_frames (H100): emit frames + manifest.json to S3
  - encode_video (L4): NVENC encode frames → MP4; fallback to libx264
  - /health endpoint for connectivity checks

## GStreamer decoder diagnostics (macOS)

- To prioritize VideoToolbox decoders ahead of libav, set `GST_PLUGIN_FEATURE_RANK` before launching the desktop app, for example `export GST_PLUGIN_FEATURE_RANK="vtdec_h264:PRIMARY+1,vtdec_hevc:PRIMARY+1"`.
- Set `GST_DECODER_DIAG=1` to emit a one-time log listing available decoder elements and their current rank—useful when debugging hardware vs. software selection.

## Stack and exact versions

Rust workspace (see Cargo.toml):
- egui = 0.29
- eframe = 0.29 (wgpu feature)
- wgpu = 0.20
- symphonia = 0.5
- crossbeam‑channel = 0.5
- walkdir = 2
- ureq = 2
- tungstenite = 0.21
- url = 2, urlencoding = 2
- native‑decoder (local crate; gstreamer feature)

External tools:
- FFmpeg/ffprobe (system) — required for import/metadata

Modal scaffold (Python):
- Python 3.10+
- modal, boto3, requests, Pillow (see modal_app/requirements.txt)
- Base images: nvidia/cuda:12.1.1‑runtime‑ubuntu22.04

## How to run (desktop)

Prereqs:
- Install Rust (stable) and FFmpeg/ffprobe on PATH

Build & run:
```bash
cargo build
cargo run --bin desktop
```

Quick tour:
- Assets → Import… to add media
- ComfyUI (local): set Repo Path (folder with main.py) and enable Auto‑import if you want local outputs to be imported automatically
- Cloud (Modal): always visible; set Base URL and API Key, choose Target (ComfyUI /prompt or Workflow (auto‑convert)), paste JSON payload, Test Connection, Queue Job
- Live job monitor: toggle per Cloud section (cloud WS) and in ComfyUI header (local WS). The app auto‑imports artifacts on job completion

Default behavior:
- ComfyUI “Open inside editor” is OFF by default
- New projects create 3 video + 3 audio baseline tracks (V1..V3, A1..A3)

## Package the desktop app

We bundle the native `desktop` binary with [`cargo-bundle`](https://github.com/burtonageo/cargo-bundle). Install the CLI once (`cargo install cargo-bundle`), run a release build, then package per platform (macOS `.app`, Windows `.exe`, Linux `.deb`/AppImage). Detailed steps live in [docs/packaging.md](docs/packaging.md).

## How to run (Modal scaffold)

```bash
# from repo root
pip install -r modal_app/requirements.txt  # for local tooling

# (one‑time) create Modal secret if you aren’t using IAM
modal secret create aws-credentials

# deploy the app
modal deploy modal_app/app.py

# smoke tests
modal run modal_app.app::health
modal run modal_app.app::generate_frames --job-id test123 --bucket your-bucket
modal run modal_app.app::encode_video --job-id test123 --bucket your-bucket --codec h264_nvenc
```

Notes:
- encode_video requires an NVENC‑capable GPU (L4/T4/A10G/RTX) and an FFmpeg build with NVENC enabled. The Dockerfile is a scaffold—replace with your NVENC build.
- generate_frames currently draws synthetic frames. Replace with a ComfyUI runner that writes real frames.

## Recent decisions (changelog‑lite)

- Tracks: default to 3 video + 3 audio; name V1..V3, A1..A3
- Local ComfyUI auto‑import: supports images + videos; true move semantics; project routing fixed; base‑path self‑heal when set to a file
- Local watcher starts when repo_path/output exists; no need to run ComfyUI embed
- Cloud section: always visible (decoupled from local embed)
- Scrolling fix: payload editor and logs now have explicit id_source to avoid egui ID collisions
- Cloud queue: POSTs to /prompt; error bodies surfaced on non‑2xx
- Cloud target: Prompt vs Workflow (auto‑wrap / best‑effort convert)
- Live monitors: local WS (/ws) and cloud WS (/events) implemented; non‑blocking stop on toggle
- Default “Open inside editor” unchecked

## Troubleshooting

- Nothing imports from ComfyUI: ensure repo_path is saved and output dir exists; ffprobe installed; check Auto‑import Logs
- Cloud job 404 on queue: Base URL points to raw ComfyUI (/prompt), not /jobs; selector should be “ComfyUI /prompt”
- 400 on /prompt: JSON must be a ComfyUI API prompt (wrap as {"prompt": {…}, "client_id": "…"}) or use Workflow target
- NVENC errors in cloud encode on H100: H100 has no NVENC; use L4 for encode or fallback to libx264

    parameters = context['parameters']

    # Your processing logic here

    return {
        "success": True,
        "output_items": [],
        "logs": ["Plugin executed successfully"]
    }
```

### Testing

```bash
# Run all tests
cargo test

# Test specific crate
cargo test --package timeline

# Run with verbose output
cargo test -- --nocapture
```

## 📋 Current Status

### ✅ Implemented

- ✅ Core timeline and project management
- ✅ GPU-accelerated preview and rendering
- ✅ Audio playback and synchronization
- ✅ Asset management with metadata
- ✅ FCPXML/FCP7/EDL export/import
- ✅ Plugin system with WASM and Python support
- ✅ Hardware encoder detection
- ✅ Command-line interface
- ✅ Cross-platform desktop application

### 🚧 In Progress / Future Features

- Advanced color grading and LUT support
- Cloud rendering service integration
- Advanced effects and transitions
- Multi-window workspace
- Collaborative editing features
- Marketplace for plugins and templates

## 🎮 Controls

### Desktop Application

- **Space**: Play/Pause
- **Mouse**: Click timeline to seek, drag clips to move/trim
- **Zoom**: Use zoom slider or "Fit" button to adjust timeline view
- **Import**: Drag files to import path field or use Import button
- **Export**: Use Export button for various output formats

### Timeline Editing

- **Click clip**: Select
- **Drag center**: Move clip
- **Drag edges**: Trim start/end
- **Snapping**: Automatic snapping to seconds and clip edges

## 🛠️ Requirements

### Minimum System Requirements

- **OS**: macOS 10.15+, Windows 10+, or Linux with OpenGL 3.3+
- **RAM**: 8 GB minimum, 16 GB recommended
- **GPU**: Any GPU with wgpu support (most modern GPUs)
- **Storage**: 2 GB free space for application and cache

### Recommended for Best Performance

- **RAM**: 32 GB or more for 4K editing
- **GPU**: Dedicated GPU with 4+ GB VRAM
- **Storage**: SSD for media files and cache
- **Hardware Encoders**: NVENC (NVIDIA), VideoToolbox (Apple), or QSV (Intel)

## 📄 License

- **Core**: MPL-2.0 (Mozilla Public License 2.0)
- **Pro Features**: Separate commercial license for advanced codecs and cloud features

## 🤝 Contributing

1. Fork the repository
2. Create a feature branch
3. Make your changes
4. Add tests for new functionality
5. Submit a pull request

## 🐛 Troubleshooting

### Common Issues

**"FFmpeg not found"**:

- Install FFmpeg and ensure it's in your PATH
- On macOS: `brew install ffmpeg`
- On Windows: Download from https://ffmpeg.org/

**"No hardware encoders detected"**:

- This is normal on some systems
- Software encoders will be used (slower but functional)
- Ensure GPU drivers are up to date

**Performance Issues**:

- Close other GPU-intensive applications
- Reduce preview resolution in timeline
- Enable proxy generation for large files

## 📞 Support

For questions, bug reports, or feature requests, please open an issue on the project repository.

---

**Built with ❤️ in Rust** | **GPU-Accelerated** | **Cross-Platform** | **Open Source**
