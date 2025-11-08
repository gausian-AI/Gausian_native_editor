use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use timeline::{Fps, Sequence};
use tracing::{info, warn};

#[derive(Parser)]
#[command(name = "gausian-cli")]
#[command(about = "Gausian Native Editor CLI - Headless video editing operations")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    #[arg(short, long, global = true)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Import media files into a project
    Import {
        /// Project file path
        #[arg(short, long)]
        project: PathBuf,

        /// Media files to import
        files: Vec<PathBuf>,

        /// Generate proxies
        #[arg(long)]
        proxies: bool,

        /// Generate thumbnails
        #[arg(long)]
        thumbnails: bool,
    },

    /// Export a sequence to video
    Export {
        /// Project file path
        #[arg(short, long)]
        project: PathBuf,

        /// Sequence name to export
        #[arg(short, long)]
        sequence: String,

        /// Output file path
        #[arg(short, long)]
        output: PathBuf,

        /// Export preset (h264-1080p, h264-720p, av1-1080p)
        #[arg(long, default_value = "h264-1080p")]
        preset: String,

        /// Custom width
        #[arg(long)]
        width: Option<u32>,

        /// Custom height
        #[arg(long)]
        height: Option<u32>,

        /// Custom bitrate in kbps
        #[arg(long)]
        bitrate: Option<u32>,
    },

    /// Convert between project formats
    Convert {
        /// Input file path
        input: PathBuf,

        /// Output file path
        output: PathBuf,

        /// Input format (auto-detected if not specified)
        #[arg(long)]
        input_format: Option<String>,

        /// Output format (fcpxml, fcp7xml, edl, json)
        #[arg(long)]
        output_format: String,
    },

    /// Analyze media files
    Analyze {
        /// Media files to analyze
        files: Vec<PathBuf>,

        /// Generate waveforms
        #[arg(long)]
        waveforms: bool,

        /// Output analysis to JSON file
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Create a new project
    New {
        /// Project name
        name: String,

        /// Project directory
        #[arg(short, long)]
        directory: Option<PathBuf>,

        /// Sequence width
        #[arg(long, default_value = "1920")]
        width: u32,

        /// Sequence height
        #[arg(long, default_value = "1080")]
        height: u32,

        /// Frame rate (e.g., 30, 25, 24)
        #[arg(long, default_value = "30")]
        fps: u32,
    },

    /// List available hardware encoders
    Encoders,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize logging
    let level = if cli.verbose {
        tracing::Level::DEBUG
    } else {
        tracing::Level::INFO
    };

    tracing_subscriber::fmt().with_max_level(level).init();

    match cli.command {
        Commands::Import {
            project,
            files,
            proxies,
            thumbnails,
        } => import_command(project, files, proxies, thumbnails).await,
        Commands::Export {
            project,
            sequence,
            output,
            preset,
            width,
            height,
            bitrate,
        } => export_command(project, sequence, output, preset, width, height, bitrate).await,
        Commands::Convert {
            input,
            output,
            input_format,
            output_format,
        } => convert_command(input, output, input_format, output_format).await,
        Commands::Analyze {
            files,
            waveforms,
            output,
        } => analyze_command(files, waveforms, output).await,
        Commands::New {
            name,
            directory,
            width,
            height,
            fps,
        } => new_command(name, directory, width, height, fps).await,
        Commands::Encoders => encoders_command().await,
    }
}

async fn import_command(
    project_path: PathBuf,
    files: Vec<PathBuf>,
    generate_proxies: bool,
    generate_thumbnails: bool,
) -> Result<()> {
    info!(
        "Importing {} files into project: {:?}",
        files.len(),
        project_path
    );

    // Create or open project
    let data_dir = project::app_data_dir();
    std::fs::create_dir_all(&data_dir)?;
    let db_path = data_dir.join("cli.db");
    let db = project::ProjectDb::open_or_create(&db_path)?;

    let project_id = project_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("cli_project");

    db.ensure_project(project_id, &format!("CLI Project: {}", project_id), None)?;

    // Import files
    for file in &files {
        if !file.exists() {
            warn!("File does not exist: {:?}", file);
            continue;
        }

        info!("Importing: {:?}", file);

        match media_io::probe_media(file) {
            Ok(info) => {
                let kind = match info.kind {
                    media_io::MediaKind::Video => "video",
                    media_io::MediaKind::Image => "image",
                    media_io::MediaKind::Audio => "audio",
                };

                let fps_num = info.fps_num.map(|v| v as i64);
                let fps_den = info.fps_den.map(|v| v as i64);
                let duration_frames = match (info.duration_seconds, fps_num, fps_den) {
                    (Some(d), Some(n), Some(dn)) if dn != 0 => {
                        Some(((d * (n as f64) / (dn as f64)).round()) as i64)
                    }
                    _ => None,
                };

                let asset_id = db.insert_asset_row(
                    project_id,
                    kind,
                    file,
                    None,
                    info.width.map(|x| x as i64),
                    info.height.map(|x| x as i64),
                    duration_frames,
                    fps_num,
                    fps_den,
                    info.audio_channels.map(|x| x as i64),
                    info.sample_rate.map(|x| x as i64),
                    info.duration_seconds,
                    None,
                    None,
                    None,
                    false,
                    false,
                    None,
                )?;

                info!("Imported {} as asset ID: {}", kind, asset_id);

                // Generate proxy if requested
                if generate_proxies && matches!(info.kind, media_io::MediaKind::Video) {
                    let proxy_path = file.with_extension("proxy.mp4");
                    info!("Generating proxy: {:?}", proxy_path);

                    if let Err(e) = media_io::generate_proxy(file, &proxy_path, 960, 540, 2000) {
                        warn!("Failed to generate proxy: {}", e);
                    } else {
                        info!("Proxy generated successfully");
                    }
                }

                // Generate thumbnail if requested
                if generate_thumbnails && matches!(info.kind, media_io::MediaKind::Video) {
                    let thumb_path = file.with_extension("thumb.jpg");
                    info!("Generating thumbnail: {:?}", thumb_path);

                    if let Err(e) = media_io::generate_thumbnail(file, &thumb_path, 1.0, 320, 180) {
                        warn!("Failed to generate thumbnail: {}", e);
                    } else {
                        info!("Thumbnail generated successfully");
                    }
                }
            }
            Err(e) => {
                warn!("Failed to probe {}: {}", file.display(), e);
            }
        }
    }

    info!("Import completed");
    Ok(())
}

async fn export_command(
    _project_path: PathBuf,
    _sequence: String,
    output: PathBuf,
    preset: String,
    width: Option<u32>,
    height: Option<u32>,
    bitrate: Option<u32>,
) -> Result<()> {
    info!("Exporting sequence to: {:?}", output);

    // Create a basic export preset
    let mut export_preset = match preset.as_str() {
        "h264-1080p" => media_io::ExportPreset::h264_1080p(),
        "h264-720p" => media_io::ExportPreset::h264_720p(),
        "av1-1080p" => media_io::ExportPreset::av1_1080p(),
        _ => {
            warn!("Unknown preset '{}', using h264-1080p", preset);
            media_io::ExportPreset::h264_1080p()
        }
    };

    // Override with custom settings
    if let Some(w) = width {
        export_preset.width = Some(w);
    }
    if let Some(h) = height {
        export_preset.height = Some(h);
    }
    if let Some(br) = bitrate {
        export_preset.video_bitrate = Some(br);
    }

    info!(
        "Using preset: {} ({}x{} at {}kbps)",
        export_preset.name,
        export_preset.width.unwrap_or(0),
        export_preset.height.unwrap_or(0),
        export_preset.video_bitrate.unwrap_or(0)
    );

    // For demo purposes, we'll just show the export would happen
    // In a real implementation, this would render the timeline
    info!("Export completed (demo mode)");

    Ok(())
}

async fn convert_command(
    input: PathBuf,
    output: PathBuf,
    _input_format: Option<String>,
    output_format: String,
) -> Result<()> {
    info!(
        "Converting {:?} to {:?} (format: {})",
        input, output, output_format
    );

    // For demo purposes, create a basic sequence
    let sequence = Sequence::new("Converted Sequence", 1920, 1080, Fps::new(30, 1), 0);
    let assets = Vec::new();

    // Create export config
    let format = match output_format.as_str() {
        "fcpxml" => exporters::ExportFormat::FcpXml1_10,
        "fcp7xml" => exporters::ExportFormat::Fcp7Xml,
        "edl" => exporters::ExportFormat::Edl,
        "json" => exporters::ExportFormat::Json,
        _ => {
            return Err(anyhow::anyhow!(
                "Unsupported output format: {}",
                output_format
            ))
        }
    };

    let config = exporters::ExportConfig {
        format,
        output_path: output.clone(),
        project_name: "CLI Conversion".to_string(),
        sequence_name: "Main".to_string(),
        relink_strategy: exporters::RelinkStrategy::Relative,
        timecode_format: exporters::TimecodeFormat::NonDropFrame,
        frame_rate: Fps::new(30, 1),
        audio_sample_rate: 48000,
        preserve_folder_structure: true,
        include_unused_media: false,
        color_space: exporters::ColorSpace::Rec709,
    };

    let exporter = exporters::Exporter::new(config);
    exporter.export_sequence(&sequence, &assets)?;

    info!("Conversion completed");
    Ok(())
}

async fn analyze_command(
    files: Vec<PathBuf>,
    generate_waveforms: bool,
    output: Option<PathBuf>,
) -> Result<()> {
    info!("Analyzing {} files", files.len());

    let mut analysis_results = Vec::new();

    for file in &files {
        if !file.exists() {
            warn!("File does not exist: {:?}", file);
            continue;
        }

        info!("Analyzing: {:?}", file);

        match media_io::probe_media(file) {
            Ok(info) => {
                let mut result = serde_json::json!({
                    "file": file,
                    "kind": format!("{:?}", info.kind),
                    "width": info.width,
                    "height": info.height,
                    "fps_num": info.fps_num,
                    "fps_den": info.fps_den,
                    "duration_seconds": info.duration_seconds,
                    "audio_channels": info.audio_channels,
                    "sample_rate": info.sample_rate,
                });

                // Generate waveform if requested and file has audio
                if generate_waveforms && (info.audio_channels.unwrap_or(0) > 0) {
                    info!("Generating waveform for: {:?}", file);
                    match media_io::generate_waveform(file, 1000) {
                        Ok(waveform) => {
                            result["waveform"] = serde_json::json!(waveform);
                            info!("Waveform generated ({} samples)", waveform.len());
                        }
                        Err(e) => {
                            warn!("Failed to generate waveform: {}", e);
                        }
                    }
                }

                analysis_results.push(result);
            }
            Err(e) => {
                warn!("Failed to analyze {}: {}", file.display(), e);
            }
        }
    }

    // Output results
    let results_json = serde_json::json!({
        "analysis_results": analysis_results,
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "total_files": files.len(),
        "analyzed_files": analysis_results.len(),
    });

    if let Some(output_path) = output {
        std::fs::write(&output_path, serde_json::to_string_pretty(&results_json)?)?;
        info!("Analysis results written to: {:?}", output_path);
    } else {
        println!("{}", serde_json::to_string_pretty(&results_json)?);
    }

    Ok(())
}

async fn new_command(
    name: String,
    directory: Option<PathBuf>,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<()> {
    let project_dir = directory.unwrap_or_else(|| PathBuf::from(&name));

    info!("Creating new project '{}' in {:?}", name, project_dir);
    info!("Sequence settings: {}x{} @ {}fps", width, height, fps);

    // Create project directory
    std::fs::create_dir_all(&project_dir)?;

    // Create basic project structure
    let data_dir = project::app_data_dir();
    std::fs::create_dir_all(&data_dir)?;
    let db_path = data_dir.join(format!("{}.db", name));
    let db = project::ProjectDb::open_or_create(&db_path)?;

    db.ensure_project(&name, &name, Some(&project_dir))?;

    // Create a basic sequence
    let sequence = Sequence::new("Main", width, height, Fps::new(fps, 1), 0);

    // Save project file
    let project_file = project_dir.join(format!("{}.gausian", name));
    let project_data = serde_json::json!({
        "name": name,
        "sequences": [sequence],
        "created": chrono::Utc::now().to_rfc3339(),
        "version": "1.0.0"
    });

    std::fs::write(&project_file, serde_json::to_string_pretty(&project_data)?)?;

    info!("Project created successfully");
    info!("Project file: {:?}", project_file);
    info!("Database: {:?}", db_path);

    Ok(())
}

async fn encoders_command() -> Result<()> {
    info!("Detecting available hardware encoders...");

    let encoders = media_io::get_hardware_encoders();

    if encoders.is_empty() {
        println!("No hardware encoders detected. Using software encoders:");
        println!("  - libx264 (H.264)");
        println!("  - libx265 (HEVC)");
        println!("  - libsvtav1 (AV1)");
    } else {
        println!("Available hardware encoders:");
        for (codec, encoder_list) in encoders {
            println!("  {}:", codec);
            for encoder in encoder_list {
                println!("    - {}", encoder);
            }
        }

        println!("\nSoftware encoders also available:");
        println!("  - libx264 (H.264)");
        println!("  - libx265 (HEVC)");
        println!("  - libsvtav1 (AV1)");
    }

    Ok(())
}
