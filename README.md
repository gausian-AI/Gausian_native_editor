<div align="center">
  <img src="apps/desktop/resources/logo_whitebg.png" width="96" alt="Gausian logo">
  <h1>Gausian Native Editor</h1>
  <p><b>Fast, native video editor and preview tool</b> built in Rust with GPU rendering, timeline editing, and local ComfyUI integration.</p>

  <p>
    <a href="#-getting-started"><b>Get Started</b></a> •
    <a href="#-features"><b>Features</b></a> •
    <a href="#-architecture"><b>Architecture</b></a> •
    <a href="#-desktop-app"><b>Desktop</b></a> •
    <a href="#-cli"><b>CLI</b></a> •
    <a href="#-decoder--gstreamer-notes"><b>Decoders</b></a>
  </p>

  <p>
    <a href="https://gausian.xyz" target="_blank" rel="noopener noreferrer"><b>Visit gausian.xyz ↗</b></a>
    &nbsp;•&nbsp;
    <a href="https://discord.gg/JfsKWDBXHT" target="_blank" rel="noopener noreferrer"><b>Join our Discord ↗</b></a>
  </p>

  <p>
    <img alt="Rust" src="https://img.shields.io/badge/Rust-stable-orange">
    <img alt="UI" src="https://img.shields.io/badge/UI-egui%20%2B%20wgpu-8A2BE2">
    <img alt="Decoders" src="https://img.shields.io/badge/Decode-VideoToolbox%2FGStreamer-2CA5E0">
    <img alt="Platforms" src="https://img.shields.io/badge/Platforms-macOS%20%7C%20Windows%20%7C%20Linux-4CAF50">
    <a href="https://discord.gg/JfsKWDBXHT" target="_blank" rel="noopener noreferrer">
      <img alt="Discord" src="https://img.shields.io/badge/Discord-Join-5865F2?logo=discord&logoColor=white">
    </a>
    <a href="https://x.com/maeng313" target="_blank" rel="noopener noreferrer">
      <img alt="Follow on X" src="https://img.shields.io/badge/X-@maeng313-272a2d?logo=x&logoColor=white">
    </a>
  </p>
</div>

<hr/>

Gausian is a native editor focused on snappy preview, practical timeline tools, and smooth ingest/export. It supports hardware decoding (VideoToolbox on macOS, GStreamer pipelines cross‑platform), a WGPU preview pipeline, and integrates with a local ComfyUI for prompt‑based generation via an embedded WebView and auto‑import of outputs. A CLI is included for headless operations.

## 👥 Contributors

[![Contributors](https://contrib.rocks/image?repo=gausian-AI/Gausian_native_editor)](https://github.com/mAengo31)

## ✨ Features

- GPU-accelerated preview (WGPU) with YUV→RGB shaders and readback
- Timeline editing, assets panel, project persistence (SQLite)
- Local ingest: FFmpeg/ffprobe probing, image/video/audio
- Exporters: FCPXML (1.9/1.10), FCP7 XML, EDL, JSON
- Proxy generation via GStreamer (ProRes/NVENC/VAAPI/software)
- Local ComfyUI: optional embedded WebView and auto‑import from a local ComfyUI output folder
- Screenplay/Storyboard helpers with LLM providers (OpenAI, etc.)
- Cross-platform desktop (macOS/Windows/Linux)

## 🚀 Getting Started

Prerequisites
- Rust (stable)
- FFmpeg/ffprobe on PATH
- GStreamer for proxy/advanced decode paths (recommended on all platforms; required for some proxies)
  - macOS (Homebrew): `brew install ffmpeg gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad gst-libav`
  - Ubuntu/Debian: `sudo apt-get install -y ffmpeg gstreamer1.0-libav gstreamer1.0-plugins-{base,good,bad} gstreamer1.0-tools`
  - Windows: install a recent GStreamer build (system PATH), FFmpeg
 - ComfyUI (local, optional): required if you want to open the embedded WebView or auto‑import its outputs. Install and run ComfyUI locally (default at http://127.0.0.1:8188). See https://github.com/comfyanonymous/ComfyUI

Desktop app
```bash
cargo run --bin desktop
```

CLI (headless)
```bash
# Show commands
cargo run -p cli -- --help
```

<!-- Relay section removed: cloud connections not available yet. -->

## 🧩 Architecture

- apps/desktop (egui + wgpu)
  - Timeline, assets, GPU preview, audio engine, export
  - ComfyUI integration (local only): optional embedded WebView and auto‑import
- apps/comfywebview
  - Minimal native WebView window for ComfyUI
- crates/*
  - timeline — graph, tracks, commands
  - project — SQLite DB, migrations, asset/proxy/job tables
  - media-io — probe/export helpers, waveforms, encoders
  - renderer — WGPU renderer and WGSL shaders
  - exporters — FCPXML/FCP7/EDL/JSON
  - plugin-host — WASM/Python stubs
  - native-decoder — VideoToolbox (macOS) + optional GStreamer backend
  - cli — import/export/convert/analyze/new/encoders

<details>
  <summary>Project Structure (click to expand)</summary>

<pre><code>apps/
  desktop/          # egui UI, preview, decode, export
  comfywebview/     # lightweight native WebView for ComfyUI

crates/
  timeline/         # timeline data structures and commands
  project/          # SQLite DB + migrations
  media-io/         # probe, waveforms, proxy helpers
  renderer/         # WGPU renderer & shaders (WGSL)
  exporters/        # FCPXML/FCP7/EDL/JSON exporters
  plugin-host/      # plugin runtime stubs (WASM/Python)
  native-decoder/   # VideoToolbox & GStreamer backend
  cli/              # headless commands

formats/            # JSON specs (screenplay/storyboard)
</code></pre>
</details>

## 🖥 Desktop App

Build & run
```bash
cargo run --bin desktop
```

Optional features
- Embedded WebView (macOS only): `cargo run --bin desktop --features embed-webview`
  - Requires a local ComfyUI installation running (default http://127.0.0.1:8188)
  - In the app, set the ComfyUI Repo Path (folder containing `main.py`) to enable local integrations

Basic flow
- Import media in Assets panel (drag drop or “Import path” + Add)
- Timeline: click to seek, drag to move/trim, snapping to seconds/edges
- Export: choose preset; FCPXML/FCP7/EDL/JSON also available
- ComfyUI (local): set Repo Path, enable auto‑import to ingest outputs
  - Note: only local ComfyUI is supported at this time; no remote/cloud connection

## 🛠 CLI

Examples
```bash
# Analyze media and print JSON
cargo run -p cli -- analyze ./media/clip.mp4 --waveforms

# List hardware encoders
cargo run -p cli -- encoders

# Convert to FCPXML/EDL/JSON (demo sequence)
cargo run -p cli -- convert in.edl out.fcpxml --output-format fcpxml
```

See all commands
```bash
cargo run -p cli -- --help
```

<!-- Relay (cloud) docs removed for now. -->

## 🎞 Proxy Encoding

Cross‑platform GStreamer pipeline with hardware profiles:
- macOS: VideoToolbox ProRes (`vtdec(_hw)` + `vtenc_prores`)
- NVIDIA: NVENC (`nvh264enc`, `nvvidconv`)
- Intel: VAAPI (`vah264enc`, `vapostproc`)
- Fallback: DNxHR (software)

macOS GStreamer (Homebrew) env (used by the app)
```bash
export GST_PLUGIN_PATH=/opt/homebrew/lib/gstreamer-1.0
export GST_PLUGIN_SYSTEM_PATH=/opt/homebrew/lib/gstreamer-1.0
export GST_PLUGIN_SCANNER=/opt/homebrew/libexec/gstreamer-1.0/gst-plugin-scanner
export GST_REGISTRY_REUSE_PLUGIN_SCANNER=no
```

## 🧪 Decoder & GStreamer Notes

- Prefer VideoToolbox on macOS; use GStreamer elsewhere (or as a macOS option)
- To force VT over libav in GStreamer, set:
```bash
export GST_PLUGIN_FEATURE_RANK="vtdec_h264:PRIMARY+1,vtdec_hevc:PRIMARY+1"
```
- One-time diagnostics:
```bash
export GST_DECODER_DIAG=1
```

## 🗺 Roadmap (Upcoming)

- Timeline polish and UX improvements
- Automatic LORA creator
- Advanced color management and LUTs
- Rich effects and transitions
- Multi‑window workspace
- Collaborative editing
- Plugin marketplace

## 🤝 Contributing

- Ensure `rustfmt`/`clippy` are green
- Keep changes focused; update docs as needed
- File issues with repro steps and logs

---

<p align="center">
  <sub>Built with Rust, egui, wgpu, GStreamer, and lots of 🧪.</sub>
</p>



## 📄 License

- **Core**: MPL-2.0 (Mozilla Public License 2.0)
- **Pro Features**: Separate commercial license for advanced codecs and pro features

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

- Join our Discord: https://discord.gg/JfsKWDBXHT

---

**Built with ❤️ in Rust** | **GPU-Accelerated** | **Cross-Platform** | **Open Source**

