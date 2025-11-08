use super::App;

pub(super) fn ensure_baseline_tracks(app: &mut App) {
    if app.seq.graph.tracks.is_empty() {
        for i in 1..=3 {
            let binding = timeline_crate::TrackBinding {
                id: timeline_crate::TrackId::new(),
                name: format!("V{}", i),
                kind: timeline_crate::TrackKind::Video,
                node_ids: Vec::new(),
            };
            let _ = app.apply_timeline_command(timeline_crate::TimelineCommand::UpsertTrack {
                track: binding,
            });
        }
        for i in 1..=3 {
            let binding = timeline_crate::TrackBinding {
                id: timeline_crate::TrackId::new(),
                name: format!("A{}", i),
                kind: timeline_crate::TrackKind::Audio,
                node_ids: Vec::new(),
            };
            let _ = app.apply_timeline_command(timeline_crate::TimelineCommand::UpsertTrack {
                track: binding,
            });
        }
        app.sync_tracks_from_graph();
    }
}

pub(super) fn load_project_timeline(app: &mut App) {
    if let Ok(Some(json)) = app.db.get_project_timeline_json(&app.project_id) {
        if let Ok(seq) = serde_json::from_str::<timeline_crate::Sequence>(&json) {
            app.seq = seq;
        } else {
            let mut seq = timeline_crate::Sequence::new(
                "Main",
                1920,
                1080,
                timeline_crate::Fps::new(30, 1),
                600,
            );
            for i in 1..=3 {
                seq.add_track(timeline_crate::Track {
                    name: format!("V{}", i),
                    items: vec![],
                });
            }
            for i in 1..=3 {
                seq.add_track(timeline_crate::Track {
                    name: format!("A{}", i),
                    items: vec![],
                });
            }
            app.seq = seq;
        }
    } else {
        let mut seq =
            timeline_crate::Sequence::new("Main", 1920, 1080, timeline_crate::Fps::new(30, 1), 600);
        for i in 1..=3 {
            seq.add_track(timeline_crate::Track {
                name: format!("V{}", i),
                items: vec![],
            });
        }
        for i in 1..=3 {
            seq.add_track(timeline_crate::Track {
                name: format!("A{}", i),
                items: vec![],
            });
        }
        app.seq = seq;
    }
    if app.seq.graph.tracks.is_empty() {
        app.seq.graph = timeline_crate::migrate_sequence_tracks(&app.seq);
    }
    super::app_timeline::sync_tracks_from_graph_impl(app);
    ensure_baseline_tracks(app);
    app.timeline_history = timeline_crate::CommandHistory::default();
    app.selected = None;
    app.drag = None;
    app.refresh_storyboard_workflows();
    app.load_storyboard_from_settings();
}

pub(super) fn save_project_timeline_impl(app: &mut App) -> anyhow::Result<()> {
    let json = serde_json::to_string(&app.seq)?;
    app.db
        .upsert_project_timeline_json(&app.project_id, &json)?;
    app.persist_storyboard_to_settings()?;
    app.last_save_at = Some(std::time::Instant::now());
    Ok(())
}

pub(super) fn save_project_timeline(app: &mut App) -> anyhow::Result<()> {
    save_project_timeline_impl(app)
}

// Thin App method wrappers to keep app.rs small
impl App {
    pub(crate) fn ensure_baseline_tracks(&mut self) {
        self::ensure_baseline_tracks(self)
    }

    pub(crate) fn load_project_timeline(&mut self) {
        self::load_project_timeline(self)
    }

    pub(crate) fn save_project_timeline(&mut self) -> anyhow::Result<()> {
        self::save_project_timeline(self)
    }

    pub(crate) fn delete_project_and_cleanup(&mut self, project_id: &str) -> anyhow::Result<()> {
        let delete_result = self.db.delete_project(project_id)?;

        let mut removal_targets: std::collections::HashSet<std::path::PathBuf> =
            std::collections::HashSet::new();

        if let Some(base) = delete_result.base_path.clone() {
            let proxy_dir = base.join("media").join("proxy");
            removal_targets.insert(proxy_dir);
            let app_dir = project::app_data_dir();
            if base.starts_with(&app_dir) {
                removal_targets.insert(base);
            }
        }

        let default_base = project::app_data_dir().join("projects").join(project_id);
        removal_targets.insert(default_base.join("media").join("proxy"));
        removal_targets.insert(default_base.clone());

        for path in delete_result.proxy_paths {
            removal_targets.insert(path);
        }

        for path in removal_targets {
            if !path.exists() {
                continue;
            }
            if path.is_dir() {
                if let Err(err) = std::fs::remove_dir_all(&path) {
                    tracing::warn!(path = %path.display(), error = %err, "failed to remove project directory entry");
                }
            } else if let Err(err) = std::fs::remove_file(&path) {
                if err.kind() == std::io::ErrorKind::IsADirectory {
                    if let Err(err2) = std::fs::remove_dir_all(&path) {
                        tracing::warn!(path = %path.display(), error = %err2, "failed to remove project directory entry");
                    }
                } else if err.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(path = %path.display(), error = %err, "failed to remove project file entry");
                }
            }
        }

        let prefix = format!("project:{}", project_id);
        let keys_to_remove: Vec<String> = self
            .asset_thumb_textures
            .keys()
            .filter(|key| key.starts_with(&prefix))
            .cloned()
            .collect();
        for key in keys_to_remove {
            if let Some(tex) = self.asset_thumb_textures.remove(&key) {
                self.textures_to_free_next_frame.push(tex);
            }
        }

        if self.project_id == project_id {
            self.project_id.clear();
            self.mode = super::AppMode::ProjectPicker;
        }

        Ok(())
    }
}
