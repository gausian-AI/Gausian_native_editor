use project::app_data_dir;
use std::io::{BufRead, BufReader};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub struct PipConfig {
    pub index_url: Option<String>,
    pub extra_index_url: Option<String>,
    pub trusted_hosts: Vec<String>,
    pub proxy: Option<String>,
    pub no_cache: bool,
}

#[derive(Clone, Debug)]
pub struct ComfyUiConfig {
    pub repo_path: Option<PathBuf>,
    pub python_cmd: String,
    pub host: String,
    pub port: u16,
    pub https: bool,
}

impl Default for ComfyUiConfig {
    fn default() -> Self {
        let python_cmd = if cfg!(target_os = "windows") {
            "python"
        } else {
            "python3"
        }
        .to_string();
        Self {
            repo_path: None,
            python_cmd,
            host: "127.0.0.1".into(),
            port: 8188,
            https: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComfyUiStatus {
    Stopped,
    Starting,
    Running,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TorchBackend {
    Auto,
    Cuda,
    Mps,
    Rocm,
    Cpu,
}

impl Default for TorchBackend {
    fn default() -> Self {
        TorchBackend::Auto
    }
}

#[derive(Clone, Debug, Default)]
pub struct InstallerPlan {
    pub install_dir: Option<PathBuf>,
    pub torch_backend: TorchBackend,
    pub pinned_ref: Option<String>, // commit or branch; None = upstream default
    pub python_for_venv: Option<String>,
    pub recreate_venv: bool,
    pub pip: PipConfig,
    // Optional: attempt to install system FFmpeg (ffmpeg/ffprobe)
    pub install_ffmpeg: bool,
}

pub struct ComfyUiManager {
    cfg: ComfyUiConfig,
    child: Option<Child>,
    pub last_status: ComfyUiStatus,
    pub last_error: Option<String>,
    log_buf: Arc<Mutex<Vec<String>>>,
    last_started_at: Option<Instant>,
    // Installer state
    pub installed_dir: Option<PathBuf>,
    pub venv_dir: Option<PathBuf>,
    pub last_pip: Option<PipConfig>,
}

impl Default for ComfyUiManager {
    fn default() -> Self {
        Self::new(ComfyUiConfig::default())
    }
}

impl ComfyUiManager {
    pub fn new(cfg: ComfyUiConfig) -> Self {
        Self {
            cfg,
            child: None,
            last_status: ComfyUiStatus::Stopped,
            last_error: None,
            log_buf: Arc::new(Mutex::new(Vec::new())),
            last_started_at: None,
            installed_dir: None,
            venv_dir: None,
            last_pip: None,
        }
    }

    pub fn config_mut(&mut self) -> &mut ComfyUiConfig {
        &mut self.cfg
    }
    pub fn config(&self) -> &ComfyUiConfig {
        &self.cfg
    }

    fn resolve_host_port(&self) -> (String, u16) {
        let mut host_input = self.cfg.host.trim();
        if host_input.is_empty() {
            return ("127.0.0.1".into(), self.cfg.port);
        }
        while host_input.ends_with('/') {
            host_input = &host_input[..host_input.len() - 1];
        }
        let parse_target = if host_input.contains("://") {
            host_input.to_string()
        } else {
            format!("http://{host_input}")
        };
        if let Ok(url) = url::Url::parse(&parse_target) {
            let host = url.host_str().unwrap_or("127.0.0.1").to_string();
            let port = url.port().unwrap_or(self.cfg.port);
            (host, port)
        } else {
            (host_input.to_string(), self.cfg.port)
        }
    }

    fn format_host_for_addr(host: &str) -> String {
        if host.contains(':') && !host.starts_with('[') && !host.ends_with(']') {
            format!("[{host}]")
        } else {
            host.to_string()
        }
    }

    pub fn set_host_input(&mut self, value: String) {
        self.cfg.host = value;
        // Update stored port if user included one in the host field.
        let mut host_input = self.cfg.host.trim();
        while host_input.ends_with('/') {
            host_input = &host_input[..host_input.len() - 1];
        }
        let parse_target = if host_input.contains("://") {
            host_input.to_string()
        } else {
            format!("http://{host_input}")
        };
        if let Ok(url) = url::Url::parse(&parse_target) {
            if let Some(port) = url.port() {
                // Clamp into the valid range; prevent zero.
                let port = port.max(1);
                self.cfg.port = port;
            }
        }
    }

    pub fn url(&self) -> String {
        let scheme = if self.cfg.https { "https" } else { "http" };
        let (host, port) = self.resolve_host_port();
        let host_fmt = Self::format_host_for_addr(&host);
        let default_port = if self.cfg.https { 443 } else { 80 };
        if port == default_port {
            format!("{scheme}://{}", host_fmt.trim_matches(['[', ']']))
        } else {
            format!("{scheme}://{host_fmt}:{port}")
        }
    }

    pub fn default_install_dir() -> PathBuf {
        app_data_dir().join("comfyui")
    }

    fn venv_python_path(venv_dir: &Path) -> PathBuf {
        if cfg!(target_os = "windows") {
            venv_dir.join("Scripts").join("python.exe")
        } else {
            // Use python3 if present; python is fine too as venv shim
            let p3 = venv_dir.join("bin").join("python3");
            if p3.exists() {
                p3
            } else {
                venv_dir.join("bin").join("python")
            }
        }
    }

    // Locate python executable of a .venv under the given repo directory, if present.
    fn find_repo_venv_python(repo: &Path) -> Option<PathBuf> {
        let vdir = repo.join(".venv");
        let vpy = Self::venv_python_path(&vdir);
        if vpy.exists() {
            Some(vpy)
        } else {
            None
        }
    }

    pub fn is_port_open(&self) -> bool {
        let (host, port) = self.resolve_host_port();
        (host.as_str(), port)
            .to_socket_addrs()
            .ok()
            .and_then(|mut it| it.next())
            .and_then(|sockaddr| {
                TcpStream::connect_timeout(&sockaddr, Duration::from_millis(150)).ok()
            })
            .is_some()
    }

    fn server_responds(&self) -> bool {
        // Best-effort probe to see if something is serving HTTP on the target URL
        // This helps distinguish a running ComfyUI from an arbitrary listener.
        // Keep timeouts short to avoid blocking UI.
        let url = format!("{}/", self.url());
        match ureq::get(&url).timeout(Duration::from_millis(700)).call() {
            Ok(resp) => resp.status() < 500,
            Err(_) => false,
        }
    }

    fn find_free_port(start: u16, attempts: u16) -> Option<u16> {
        let mut p = start.max(1024);
        for _ in 0..attempts {
            if let Ok(listener) = TcpListener::bind(("127.0.0.1", p)) {
                // Successfully bound means it's free; drop immediately and use it.
                drop(listener);
                return Some(p);
            }
            p = p.saturating_add(1);
            if p > 65535 {
                p = 1024;
            }
        }
        None
    }

    pub fn is_running(&mut self) -> bool {
        // First check the child if we started it
        if let Some(child) = self.child.as_mut() {
            if let Ok(None) = child.try_wait() {
                return true;
            }
        }
        // Otherwise, check whether a server responds on the port
        self.is_port_open()
    }

    pub fn start(&mut self) {
        if self.is_running() {
            self.last_status = ComfyUiStatus::Running;
            self.last_error = None;
            return;
        }

        // If the configured port is already in use, either reuse an existing ComfyUI
        // or automatically switch to the next available port to avoid bind errors.
        if self.is_port_open() {
            if self.server_responds() {
                self.log("Detected existing ComfyUI on configured port; reusing.");
                self.last_status = ComfyUiStatus::Running;
                self.last_error = None;
                return;
            } else if let Some(free) = Self::find_free_port(self.cfg.port.saturating_add(1), 200) {
                let old = self.cfg.port;
                self.cfg.port = free;
                self.log(&format!(
                    "Port {} is busy; switching to available port {}",
                    old, free
                ));
            } else {
                self.last_status = ComfyUiStatus::Error;
                self.last_error = Some("No free local ports found (range scanned)".into());
                return;
            }
        }

        self.last_status = ComfyUiStatus::Starting;
        self.last_error = None;

        let repo = match &self.cfg.repo_path {
            Some(p) if p.join("main.py").exists() => p.clone(),
            _ => {
                self.last_status = ComfyUiStatus::Error;
                self.last_error = Some("ComfyUI repo path not set or main.py missing".into());
                return;
            }
        };

        // Prefer the repo's venv python if available; otherwise use configured python
        let py_cmd = Self::find_repo_venv_python(&repo)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| self.cfg.python_cmd.clone());
        let mut cmd = Command::new(&py_cmd);
        cmd.arg("main.py")
            .current_dir(&repo)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Try to set port via known flag; if unsupported, ComfyUI will log an error.
        cmd.arg("--port").arg(self.cfg.port.to_string());

        match cmd.spawn() {
            Ok(mut child) => {
                self.last_started_at = Some(Instant::now());
                let out = child.stdout.take();
                let err = child.stderr.take();
                let log_buf = self.log_buf.clone();
                if let Some(out) = out {
                    thread::spawn(move || {
                        let reader = BufReader::new(out);
                        for line in reader.lines().flatten() {
                            if let Ok(mut b) = log_buf.lock() {
                                b.push(format!("[O] {}", line));
                            }
                        }
                    });
                }
                let log_buf = self.log_buf.clone();
                if let Some(err) = err {
                    thread::spawn(move || {
                        let reader = BufReader::new(err);
                        for line in reader.lines().flatten() {
                            if let Ok(mut b) = log_buf.lock() {
                                b.push(format!("[E] {}", line));
                            }
                        }
                    });
                }
                self.child = Some(child);

                // Poll port readiness briefly
                let deadline = Instant::now() + Duration::from_secs(8);
                while Instant::now() < deadline {
                    if self.is_port_open() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(150));
                }
                self.last_status = if self.is_running() {
                    ComfyUiStatus::Running
                } else {
                    ComfyUiStatus::Starting
                };
            }
            Err(e) => {
                self.last_status = ComfyUiStatus::Error;
                self.last_error = Some(format!("Failed to spawn ComfyUI: {}", e));
            }
        }
    }

    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.last_status = ComfyUiStatus::Stopped;
    }

    pub fn logs(&self, max_lines: usize) -> Vec<String> {
        let buf = self.log_buf.lock().unwrap();
        let n = buf.len();
        let start = n.saturating_sub(max_lines);
        buf[start..].to_vec()
    }

    pub fn open_webview_window(&self) {
        // Try to spawn the helper binary if it's in PATH; fall back to system browser.
        let url = self.url();
        if Command::new("comfywebview")
            .arg("--url")
            .arg(&url)
            .arg("--title")
            .arg("ComfyUI")
            .spawn()
            .is_err()
        {
            let _ = webbrowser::open(&url);
        }
    }

    fn log(&self, s: impl Into<String>) {
        if let Ok(mut b) = self.log_buf.lock() {
            b.push(s.into());
        }
    }

    fn apply_pip_config<'a>(base: &[&'a str], pip: &PipConfig) -> Vec<String> {
        let mut v: Vec<String> = base.iter().map(|s| s.to_string()).collect();
        if let Some(ix) = &pip.index_url {
            v.push("-i".into());
            v.push(ix.clone());
        }
        if let Some(ex) = &pip.extra_index_url {
            v.push("--extra-index-url".into());
            v.push(ex.clone());
        }
        for h in &pip.trusted_hosts {
            v.push("--trusted-host".into());
            v.push(h.clone());
        }
        if let Some(p) = &pip.proxy {
            v.push("--proxy".into());
            v.push(p.clone());
        }
        if pip.no_cache {
            v.push("--no-cache-dir".into());
        }
        v
    }

    fn torch_args(backend: TorchBackend) -> Vec<&'static str> {
        match backend {
            TorchBackend::Auto => {
                if cfg!(target_os = "macos") {
                    // Apple Silicon MPS is default in upstream wheels
                    vec!["torch", "torchvision", "torchaudio"]
                } else if cfg!(target_os = "windows") {
                    // Default to CUDA 12.1 build; users without NVIDIA will fall back later
                    vec![
                        "--index-url",
                        "https://download.pytorch.org/whl/cu121",
                        "torch",
                        "torchvision",
                        "torchaudio",
                    ]
                } else {
                    // Linux: default to CPU to avoid driver mismatch surprises; users can pick CUDA/ROCm explicitly
                    vec![
                        "--index-url",
                        "https://download.pytorch.org/whl/cpu",
                        "torch",
                        "torchvision",
                        "torchaudio",
                    ]
                }
            }
            TorchBackend::Cuda => vec![
                "--index-url",
                "https://download.pytorch.org/whl/cu121",
                "torch",
                "torchvision",
                "torchaudio",
            ],
            TorchBackend::Mps => vec!["torch", "torchvision", "torchaudio"],
            TorchBackend::Rocm => vec![
                "--index-url",
                "https://download.pytorch.org/whl/rocm5.6",
                "torch",
                "torchvision",
                "torchaudio",
            ],
            TorchBackend::Cpu => vec![
                "--index-url",
                "https://download.pytorch.org/whl/cpu",
                "torch",
                "torchvision",
                "torchaudio",
            ],
        }
    }

    pub fn install(&mut self, plan: InstallerPlan) {
        // Spawn a thread for the install so UI stays responsive.
        let log_buf = self.log_buf.clone();
        let mut plan = plan.clone();
        if plan.install_dir.is_none() {
            plan.install_dir = Some(Self::default_install_dir());
        }
        let install_dir = plan.install_dir.unwrap();
        // Record intended install directory immediately so subsequent actions (e.g., Validate/Use Installed)
        // know where to look even before the background thread finishes.
        self.installed_dir = Some(install_dir.clone());
        let torch_backend = plan.torch_backend;
        let pinned_ref = plan.pinned_ref.clone();
        let mut mgr = self.clone_for_install();
        let pip_cfg = plan.pip.clone();
        let want_ffmpeg = plan.install_ffmpeg;
        thread::spawn(move || {
            let log = |s: &str| {
                if let Ok(mut b) = log_buf.lock() {
                    b.push(s.to_string());
                }
            };
            let run = |program: &str, args: &[&str], cwd: Option<&Path>| -> std::io::Result<i32> {
                log(&format!("$ {} {}", program, args.join(" ")));
                let mut c = Command::new(program);
                c.args(args);
                if let Some(d) = cwd {
                    c.current_dir(d);
                }
                c.stdout(Stdio::piped()).stderr(Stdio::piped());
                let mut child = c.spawn()?;
                if let Some(out) = child.stdout.take() {
                    let lb = log_buf.clone();
                    thread::spawn(move || {
                        for l in BufReader::new(out).lines().flatten() {
                            if let Ok(mut b) = lb.lock() {
                                b.push(format!("[O] {}", l));
                            }
                        }
                    });
                }
                if let Some(err) = child.stderr.take() {
                    let lb = log_buf.clone();
                    thread::spawn(move || {
                        for l in BufReader::new(err).lines().flatten() {
                            if let Ok(mut b) = lb.lock() {
                                b.push(format!("[E] {}", l));
                            }
                        }
                    });
                }
                let status = child.wait()?;
                Ok(status.code().unwrap_or(-1))
            };

            // Ensure install folder exists
            std::fs::create_dir_all(&install_dir).ok();

            // Clone if main.py missing
            let has_main = install_dir.join("main.py").exists();
            if !has_main {
                let code = run(
                    "git",
                    &[
                        "clone",
                        "https://github.com/comfyanonymous/ComfyUI",
                        install_dir.to_string_lossy().as_ref(),
                    ],
                    None,
                )
                .unwrap_or(-1);
                if code != 0 {
                    log("git clone failed");
                    return;
                }
            }
            // Checkout pinned ref if provided
            if let Some(r) = pinned_ref.as_deref() {
                let _ = run("git", &["fetch", "--all"], Some(&install_dir));
                let code = run("git", &["checkout", r], Some(&install_dir)).unwrap_or(-1);
                if code != 0 {
                    log("git checkout failed");
                }
            }

            // Create venv if missing
            let venv_dir = install_dir.join(".venv");
            if plan.recreate_venv && venv_dir.exists() {
                let _ = std::fs::remove_dir_all(&venv_dir);
                log("Removed existing .venv as requested (recreate)");
            }
            if !venv_dir.exists() {
                // Pick a python interpreter for venv creation (prefer <= 3.12 for torch support)
                #[cfg(target_os = "windows")]
                let (venv_prog, venv_prefix): (String, Vec<&str>) = {
                    let mut prog = "python".to_string();
                    // If user configured a specific python, try it first
                    if let Some(p) = plan.python_for_venv.as_ref() {
                        if !p.trim().is_empty() {
                            prog = p.clone();
                        }
                    } else if !mgr.cfg.python_cmd.trim().is_empty() {
                        prog = mgr.cfg.python_cmd.clone();
                    }
                    (prog, vec![])
                };
                #[cfg(not(target_os = "windows"))]
                let (venv_prog, venv_prefix): (String, Vec<&str>) = {
                    let mut candidates: Vec<String> = Vec::new();
                    if let Some(p) = plan.python_for_venv.as_ref() {
                        if !p.trim().is_empty() {
                            candidates.push(p.clone());
                        }
                    }
                    if !mgr.cfg.python_cmd.trim().is_empty() {
                        candidates.push(mgr.cfg.python_cmd.clone());
                    }
                    candidates.extend([
                        "python3.12".to_string(),
                        "python3.11".to_string(),
                        "python3.10".to_string(),
                        "python3".to_string(),
                        "python".to_string(),
                    ]);
                    let mut chosen = None;
                    for c in candidates.into_iter() {
                        if let Ok(out) = std::process::Command::new(&c)
                            .arg("-c")
                            .arg(
                                "import sys; print(f'{sys.version_info[0]}.{sys.version_info[1]}')",
                            )
                            .output()
                        {
                            if out.status.success() {
                                if let Ok(s) = String::from_utf8(out.stdout) {
                                    let s = s.trim();
                                    let parts: Vec<_> = s.split('.').collect();
                                    if parts.len() >= 2 {
                                        if let (Ok(maj), Ok(min)) =
                                            (parts[0].parse::<i32>(), parts[1].parse::<i32>())
                                        {
                                            if maj == 3 && (10..=12).contains(&min) {
                                                chosen = Some(c);
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    (chosen.unwrap_or_else(|| "python3".to_string()), vec![])
                };
                let mut args: Vec<&str> = Vec::new();
                args.extend(venv_prefix.iter().copied());
                args.extend(["-m", "venv", ".venv"].iter().copied());
                let code = run(&venv_prog, &args, Some(&install_dir)).unwrap_or(-1);
                if code != 0 {
                    log("venv creation failed");
                    return;
                }
            } else {
                log("Reusing existing virtual environment (.venv)");
            }
            let vpy = Self::venv_python_path(&venv_dir);
            if !vpy.exists() {
                log("venv python not found");
                return;
            }
            // Warn if Python is unsupported for torch (e.g., 3.13+)
            if let Ok(out) = std::process::Command::new(&vpy)
                .arg("-c")
                .arg("import sys; print(f'{sys.version_info[0]}.{sys.version_info[1]}')")
                .output()
            {
                if out.status.success() {
                    if let Ok(s) = String::from_utf8(out.stdout) {
                        let s = s.trim();
                        let parts: Vec<_> = s.split('.').collect();
                        if parts.len() >= 2 {
                            if let (Ok(maj), Ok(min)) =
                                (parts[0].parse::<i32>(), parts[1].parse::<i32>())
                            {
                                if !(maj == 3 && (10..=12).contains(&min)) {
                                    log("Warning: Python version is not in 3.10–3.12; PyTorch wheels may be unavailable.");
                                }
                            }
                        }
                    }
                }
            }
            // Upgrade pip/setuptools/wheel (allow slower networks) with configured indexes
            let up_base = [
                "-m",
                "pip",
                "install",
                "--upgrade",
                "pip",
                "setuptools",
                "wheel",
                "--timeout",
                "60",
            ];
            let up_args = ComfyUiManager::apply_pip_config(&up_base, &pip_cfg);
            let up_args_str: Vec<&str> = up_args.iter().map(|s| s.as_str()).collect();
            let _ = run(
                vpy.to_string_lossy().as_ref(),
                &up_args_str,
                Some(&install_dir),
            );
            // Install torch (selected backend)
            // Torch install: if user provided index_url, prefer it over backend defaults
            let mut torch: Vec<String> = ComfyUiManager::apply_pip_config(
                &["-m", "pip", "install", "--timeout", "90"],
                &pip_cfg,
            );
            if pip_cfg.index_url.is_none() && pip_cfg.extra_index_url.is_none() {
                let args = Self::torch_args(torch_backend);
                for a in args {
                    torch.push(a.to_string());
                }
            } else {
                torch.push("torch".into());
                torch.push("torchvision".into());
                torch.push("torchaudio".into());
            }
            let torch_args: Vec<&str> = torch.iter().map(|s| s.as_str()).collect();
            let torch_code = run(
                vpy.to_string_lossy().as_ref(),
                &torch_args,
                Some(&install_dir),
            )
            .unwrap_or(-1);
            if torch_code != 0 {
                log("Torch install failed. If using Python 3.13+, please switch to Python 3.11/3.12 and re-run Install.");
                // Fallback attempt to CPU wheels if not already CPU
                if !matches!(torch_backend, TorchBackend::Cpu) {
                    log("Falling back to CPU torch wheels...");
                    let mut torch_cpu: Vec<String> = ComfyUiManager::apply_pip_config(
                        &["-m", "pip", "install", "--timeout", "90"],
                        &pip_cfg,
                    );
                    let args = Self::torch_args(TorchBackend::Cpu);
                    for a in args {
                        torch_cpu.push(a.to_string());
                    }
                    let torch_cpu_args: Vec<&str> = torch_cpu.iter().map(|s| s.as_str()).collect();
                    let _ = run(
                        vpy.to_string_lossy().as_ref(),
                        &torch_cpu_args,
                        Some(&install_dir),
                    );
                }
            }
            // Install ComfyUI requirements with a retry (common network hiccups)
            let req_base = [
                "-m",
                "pip",
                "install",
                "-r",
                "requirements.txt",
                "--timeout",
                "90",
            ];
            let req_args_s = ComfyUiManager::apply_pip_config(&req_base, &pip_cfg);
            let req_args: Vec<&str> = req_args_s.iter().map(|s| s.as_str()).collect();
            let code = run(
                vpy.to_string_lossy().as_ref(),
                &req_args,
                Some(&install_dir),
            )
            .unwrap_or(-1);
            if code != 0 {
                log("requirements install failed; retrying once...");
                let _ = run(
                    vpy.to_string_lossy().as_ref(),
                    &req_args,
                    Some(&install_dir),
                );
            }
            // Ensure commonly-missed extras present
            let py_base = ["-m", "pip", "install", "pyyaml", "--timeout", "60"];
            let py_args_s = ComfyUiManager::apply_pip_config(&py_base, &pip_cfg);
            let py_args: Vec<&str> = py_args_s.iter().map(|s| s.as_str()).collect();
            let _ = run(vpy.to_string_lossy().as_ref(), &py_args, Some(&install_dir));

            // Persist install in manager
            mgr.installed_dir = Some(install_dir.clone());
            mgr.venv_dir = Some(venv_dir.clone());
            mgr.cfg.repo_path = Some(install_dir.clone());
            mgr.cfg.python_cmd = vpy.to_string_lossy().to_string();
            // Final sanity import for yaml; if missing try once more
            let mut c = Command::new(vpy.to_string_lossy().to_string());
            c.current_dir(&install_dir)
                .arg("-c").arg("import sys;\ntry:\n import yaml; print('yaml_ok')\n sys.exit(0)\nexcept Exception as e:\n print('yaml_missing', e); sys.exit(0)");
            if let Ok(mut child) = c.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
                if let Some(out) = child.stdout.take() {
                    for l in BufReader::new(out).lines().flatten() {
                        if l.contains("yaml_missing") {
                            let _ =
                                run(vpy.to_string_lossy().as_ref(), &py_args, Some(&install_dir));
                        }
                    }
                }
                let _ = child.wait();
            }
            // Optional: install FFmpeg (system-wide) if requested
            if want_ffmpeg {
                let ff_ok = std::process::Command::new("ffprobe")
                    .arg("-version")
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false);
                if ff_ok {
                    log("FFmpeg already present (ffprobe found)");
                } else {
                    log("FFmpeg not found; attempting system install...");
                    #[cfg(target_os = "macos")]
                    {
                        let brew_ok = std::process::Command::new("brew")
                            .arg("--version")
                            .output()
                            .map(|o| o.status.success())
                            .unwrap_or(false);
                        if brew_ok {
                            let _ = run("brew", &["update"], None);
                            let code = run("brew", &["install", "ffmpeg"], None).unwrap_or(-1);
                            if code == 0 {
                                log("FFmpeg installed via Homebrew");
                            } else {
                                log("Homebrew install failed; please install FFmpeg manually")
                            }
                        } else {
                            log("Homebrew not found; install it from https://brew.sh or install FFmpeg manually from https://ffmpeg.org");
                        }
                    }
                    #[cfg(target_os = "windows")]
                    {
                        let winget_ok = std::process::Command::new("winget")
                            .arg("--version")
                            .output()
                            .map(|o| o.status.success())
                            .unwrap_or(false);
                        if winget_ok {
                            let mut ok = run(
                                "winget",
                                &["install", "--id=FFmpeg.FFmpeg", "-e", "--source", "winget"],
                                None,
                            )
                            .unwrap_or(-1)
                                == 0;
                            if !ok {
                                ok = run(
                                    "winget",
                                    &["install", "--id=Gyan.FFmpeg", "-e", "--source", "winget"],
                                    None,
                                )
                                .unwrap_or(-1)
                                    == 0;
                            }
                            if ok {
                                log("FFmpeg installed via winget");
                            } else {
                                log("winget install failed; trying other managers or install manually");
                            }
                        } else {
                            let choco_ok = std::process::Command::new("choco")
                                .arg("-v")
                                .output()
                                .map(|o| o.status.success())
                                .unwrap_or(false);
                            if choco_ok {
                                let _ = run("choco", &["install", "ffmpeg", "-y"], None);
                            } else {
                                let scoop_ok = std::process::Command::new("scoop")
                                    .arg("-v")
                                    .output()
                                    .map(|o| o.status.success())
                                    .unwrap_or(false);
                                if scoop_ok {
                                    let _ = run("scoop", &["install", "ffmpeg"], None);
                                } else {
                                    log("No package manager found (winget/choco/scoop). Please install FFmpeg from https://ffmpeg.org");
                                }
                            }
                        }
                    }
                    #[cfg(target_os = "linux")]
                    {
                        let have = |cmd: &str| {
                            std::process::Command::new(cmd)
                                .arg("--version")
                                .output()
                                .map(|o| o.status.success())
                                .unwrap_or(false)
                        };
                        let try_sudo =
                            |args: &[&str]| -> bool { run("sudo", args, None).unwrap_or(-1) == 0 };
                        if have("ffprobe") {
                            // Installed meanwhile
                        } else if have("apt-get") {
                            let _ = try_sudo(&["-n", "apt-get", "update"]);
                            let _ = try_sudo(&["-n", "apt-get", "install", "-y", "ffmpeg"]);
                        } else if have("dnf") {
                            let _ = try_sudo(&["-n", "dnf", "install", "-y", "ffmpeg"]);
                        } else if have("pacman") {
                            let _ = try_sudo(&["-n", "pacman", "-S", "--noconfirm", "ffmpeg"]);
                        } else if have("zypper") {
                            let _ = try_sudo(&["-n", "zypper", "install", "-y", "ffmpeg"]);
                        } else {
                            log("Unknown Linux package manager; please install FFmpeg (ffmpeg, ffprobe) via your distro");
                        }
                    }
                }
            }

            log("Install/Repair complete");
            // Persist pip config for future validate/repairs
            mgr.last_pip = Some(pip_cfg);
        });
    }

    pub fn validate_install(&mut self) {
        // Validate current repo/python OR installed venv if available.
        let repo = self
            .cfg
            .repo_path
            .clone()
            .or(self.installed_dir.clone())
            .unwrap_or_else(|| Self::default_install_dir());
        // Prefer repo .venv python; then recorded venv_dir; otherwise configured python
        let py_path: String = if let Some(vpy) = Self::find_repo_venv_python(&repo) {
            vpy.to_string_lossy().to_string()
        } else if let Some(v) = self.venv_dir.as_ref() {
            Self::venv_python_path(v).to_string_lossy().to_string()
        } else {
            self.cfg.python_cmd.clone()
        };
        let log_buf = self.log_buf.clone();
        let pip_cfg = self.last_pip.clone().unwrap_or_default();
        thread::spawn(move || {
            let log = |s: &str| {
                if let Ok(mut b) = log_buf.lock() {
                    b.push(s.to_string());
                }
            };
            let mut c = Command::new(&py_path);
            c.current_dir(&repo)
                .arg("-c")
                .arg("import sys,platform;\nprint('py', sys.version)\nprint('arch', platform.machine())\nprint('64bit', sys.maxsize>2**32)\ntry:\n import torch\n print('torch', torch.__version__)\n print('cuda', torch.cuda.is_available())\n print('mps', hasattr(torch.backends,'mps') and torch.backends.mps.is_available())\nexcept Exception as e:\n print('torch_err', e)\n sys.exit(0)");
            c.stdout(Stdio::piped()).stderr(Stdio::piped());
            match c.spawn() {
                Ok(mut child) => {
                    if let Some(out) = child.stdout.take() {
                        let lb = log_buf.clone();
                        thread::spawn(move || {
                            for l in BufReader::new(out).lines().flatten() {
                                if let Ok(mut b) = lb.lock() {
                                    b.push(format!("[VAL] {}", l));
                                }
                            }
                        });
                    }
                    if let Some(err) = child.stderr.take() {
                        let lb = log_buf.clone();
                        thread::spawn(move || {
                            for l in BufReader::new(err).lines().flatten() {
                                if let Ok(mut b) = lb.lock() {
                                    b.push(format!("[VAL-ERR] {}", l));
                                }
                            }
                        });
                    }
                    let _ = child.wait();
                    log("Validation finished");
                }
                Err(e) => {
                    log(&format!("Validation failed to spawn: {}", e));
                }
            }
            // pip index versions torch
            let mut pip_cmd = Command::new(&py_path);
            let base = ["-m", "pip", "index", "versions", "torch"];
            let opts = ComfyUiManager::apply_pip_config(&base, &pip_cfg);
            let args: Vec<&str> = opts.iter().map(|s| s.as_str()).collect();
            pip_cmd
                .current_dir(&repo)
                .args(&args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            match pip_cmd.spawn() {
                Ok(mut child) => {
                    if let Some(out) = child.stdout.take() {
                        let lb = log_buf.clone();
                        thread::spawn(move || {
                            for l in BufReader::new(out).lines().flatten() {
                                if let Ok(mut b) = lb.lock() {
                                    b.push(format!("[PIP] {}", l));
                                }
                            }
                        });
                    }
                    if let Some(err) = child.stderr.take() {
                        let lb = log_buf.clone();
                        thread::spawn(move || {
                            for l in BufReader::new(err).lines().flatten() {
                                if let Ok(mut b) = lb.lock() {
                                    b.push(format!("[PIP-ERR] {}", l));
                                }
                            }
                        });
                    }
                    let _ = child.wait();
                }
                Err(e) => {
                    log(&format!("pip index versions failed: {}", e));
                }
            }
        });
    }

    pub fn use_installed(&mut self) {
        // Prefer previously recorded install dir; otherwise use default if it looks valid
        let mut dir = self.installed_dir.clone();
        if dir.is_none() {
            let def = Self::default_install_dir();
            if def.join("main.py").exists() {
                dir = Some(def);
            }
        }
        if let Some(d) = dir.clone() {
            self.cfg.repo_path = Some(d.clone());
        }
        // Prefer recorded venv; otherwise detect venv under chosen dir
        let mut venv = self.venv_dir.clone();
        if venv.is_none() {
            if let Some(d) = dir.as_ref() {
                let v = d.join(".venv");
                if Self::venv_python_path(&v).exists() {
                    venv = Some(v);
                }
            }
        }
        if let Some(v) = venv {
            self.venv_dir = Some(v.clone());
            let vpy = Self::venv_python_path(&v);
            self.cfg.python_cmd = vpy.to_string_lossy().to_string();
        }
    }

    pub fn repair_common_packages(&mut self) {
        // Check and install a common set of packages in the selected environment
        let repo = self
            .cfg
            .repo_path
            .clone()
            .or(self.installed_dir.clone())
            .unwrap_or_else(|| Self::default_install_dir());
        // Prefer repo .venv python; then recorded venv_dir; otherwise configured python
        let py_path: String = if let Some(vpy) = Self::find_repo_venv_python(&repo) {
            vpy.to_string_lossy().to_string()
        } else if let Some(v) = self.venv_dir.as_ref() {
            Self::venv_python_path(v).to_string_lossy().to_string()
        } else {
            self.cfg.python_cmd.clone()
        };
        let log_buf = self.log_buf.clone();
        let pip_cfg = self.last_pip.clone().unwrap_or_default();
        thread::spawn(move || {
            let log = |s: &str| {
                if let Ok(mut b) = log_buf.lock() {
                    b.push(s.to_string());
                }
            };
            let check_script = r#"
missing = []
mods = {
    'yaml':'pyyaml',
    'PIL':'pillow',
    'numpy':'numpy',
    'aiohttp':'aiohttp',
    'fastapi':'fastapi',
    'uvicorn':'uvicorn',
    'pydantic':'pydantic',
}
for m in list(mods.keys()):
    try:
        __import__(m)
    except Exception:
        missing.append((m, mods[m]))
print('MISSING:', ','.join(p for m,p in missing))
"#;
            // Run check
            let mut c = Command::new(&py_path);
            c.current_dir(&repo).arg("-c").arg(check_script);
            match c.output() {
                Ok(out) => {
                    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                    let line = stdout.lines().find(|l| l.starts_with("MISSING:"));
                    let mut pkgs: Vec<String> = Vec::new();
                    if let Some(l) = line {
                        let list = l.trim_start_matches("MISSING:").trim();
                        if !list.is_empty() {
                            pkgs = list
                                .split(',')
                                .filter(|s| !s.trim().is_empty())
                                .map(|s| s.trim().to_string())
                                .collect();
                        }
                    }
                    if pkgs.is_empty() {
                        log("Repair: no missing common packages detected");
                        return;
                    }
                    log(&format!(
                        "Repair: installing missing packages: {}",
                        pkgs.join(", ")
                    ));
                    // pip install missing
                    let mut cmd = Command::new(&py_path);
                    let mut args: Vec<String> = ComfyUiManager::apply_pip_config(
                        &["-m", "pip", "install", "--timeout", "60"],
                        &pip_cfg,
                    );
                    for p in &pkgs {
                        args.push(p.clone());
                    }
                    cmd.current_dir(&repo).args(args);
                    match cmd.status() {
                        Ok(s) => {
                            if s.success() {
                                log("Repair: installation finished");
                            } else {
                                log(&format!("Repair: pip exited with status {}", s));
                            }
                        }
                        Err(e) => log(&format!("Repair: failed to spawn pip: {}", e)),
                    }
                }
                Err(e) => log(&format!("Repair: check failed to run: {}", e)),
            }
        });
    }

    pub fn uninstall(&mut self) {
        if let Some(dir) = self.installed_dir.take() {
            let _ = std::fs::remove_dir_all(&dir);
            self.venv_dir = None;
            self.log("Uninstalled ComfyUI directory");
        }
    }

    fn clone_for_install(&self) -> Self {
        Self {
            cfg: self.cfg.clone(),
            child: None,
            last_status: self.last_status,
            last_error: None,
            log_buf: self.log_buf.clone(),
            last_started_at: None,
            installed_dir: self.installed_dir.clone(),
            venv_dir: self.venv_dir.clone(),
            last_pip: self.last_pip.clone(),
        }
    }
}
