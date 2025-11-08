use anyhow::Result;
use rusqlite::{params, Connection, Transaction};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub fn app_data_dir() -> PathBuf {
    let base = dirs::data_local_dir().unwrap_or_else(|| std::env::temp_dir());
    base.join("gausian_native")
}

pub struct ProjectDb {
    conn: Connection,
    path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ProjectDeleteResult {
    pub base_path: Option<PathBuf>,
    pub proxy_paths: Vec<PathBuf>,
}

impl ProjectDb {
    pub fn begin_tx(&self) -> Result<Transaction<'_>> {
        Ok(self.conn.unchecked_transaction()?)
    }

    pub fn upsert_asset_fast(
        &self,
        project_id: &str,
        kind: &str,
        src_abs: &Path,
    ) -> Result<String> {
        self.insert_asset_row(
            project_id, kind, src_abs, None, None, None, None, None, None, None, None, None, None,
            None, None, false, false, None,
        )
    }

    pub fn mark_asset_ready(&self, asset_id: &str, ready: bool) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "UPDATE assets SET notes = ?2, updated_at = ?3 WHERE id = ?1",
            params![asset_id, if ready { "ready" } else { "pending" }, now],
        )?;
        Ok(())
    }

    pub fn update_asset_analysis(
        &self,
        asset_id: &str,
        waveform_path: Option<&Path>,
        thumbs_path: Option<&Path>,
        proxy_path: Option<&Path>,
        seek_index_path: Option<&Path>,
    ) -> Result<()> {
        if let Some(p) = waveform_path {
            self.conn.execute("INSERT OR REPLACE INTO cache(id, asset_id, kind, path_abs, created_at) VALUES(?1, ?2, 'waveform', ?3, strftime('%s','now'))", params![format!("wf-{}", asset_id), asset_id, p.to_string_lossy()])?;
        }
        if let Some(p) = thumbs_path {
            self.conn.execute("INSERT OR REPLACE INTO cache(id, asset_id, kind, path_abs, created_at) VALUES(?1, ?2, 'thumbnail', ?3, strftime('%s','now'))", params![format!("th-{}", asset_id), asset_id, p.to_string_lossy()])?;
        }
        if let Some(p) = proxy_path {
            self.conn.execute("INSERT OR REPLACE INTO proxies(id, asset_id, kind, path_abs, settings_hash, created_at) VALUES(?1, ?2, 'proxy', ?3, 'default', strftime('%s','now'))", params![format!("px-{}", asset_id), asset_id, p.to_string_lossy()])?;
        }
        if let Some(p) = seek_index_path {
            self.conn.execute("INSERT OR REPLACE INTO cache(id, asset_id, kind, path_abs, created_at) VALUES(?1, ?2, 'analysis', ?3, strftime('%s','now'))", params![format!("sk-{}", asset_id), asset_id, p.to_string_lossy()])?;
        }
        Ok(())
    }

    pub fn update_asset_metadata(
        &self,
        asset_id: &str,
        metadata: &serde_json::Value,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "UPDATE assets SET metadata_json = ?2, updated_at = ?3 WHERE id = ?1",
            params![asset_id, metadata.to_string(), now],
        )?;
        Ok(())
    }

    pub fn enqueue_job(
        &self,
        job_id: &str,
        asset_id: &str,
        kind: &str,
        priority: i32,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO jobs(id, asset_id, kind, priority, status, created_at, updated_at) VALUES(?1, ?2, ?3, ?4, 'pending', strftime('%s','now'), strftime('%s','now'))",
            params![job_id, asset_id, kind, priority],
        )?;
        Ok(())
    }

    pub fn update_job_status(&self, job_id: &str, status: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE jobs SET status = ?2, updated_at = strftime('%s','now') WHERE id = ?1",
            params![job_id, status],
        )?;
        Ok(())
    }

    pub fn get_project_settings_json(&self, project_id: &str) -> Result<serde_json::Value> {
        let mut stmt = self
            .conn
            .prepare("SELECT settings_json FROM projects WHERE id = ?1 LIMIT 1")?;
        let mut rows = stmt.query(params![project_id])?;
        if let Some(row) = rows.next()? {
            let raw: String = row.get(0)?;
            let parsed = serde_json::from_str(&raw)
                .unwrap_or_else(|_| serde_json::Value::Object(Default::default()));
            Ok(parsed)
        } else {
            Ok(serde_json::Value::Object(Default::default()))
        }
    }

    pub fn update_project_settings_json(
        &self,
        project_id: &str,
        settings: &serde_json::Value,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "UPDATE projects SET settings_json = ?2, updated_at = ?3 WHERE id = ?1",
            params![project_id, settings.to_string(), now],
        )?;
        Ok(())
    }
    pub fn open_or_create(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        // Recommended PRAGMAs for local interactive app DB
        conn.pragma_update(None, "journal_mode", &"WAL")?;
        conn.pragma_update(None, "synchronous", &"NORMAL")?;
        conn.pragma_update(None, "foreign_keys", &"ON")?;
        // Optional cache/mmap tuning (safe defaults if unsupported)
        let _ = conn.pragma_update(None, "mmap_size", &"134217728"); // 128MB
        let _ = conn.pragma_update(None, "cache_size", &"-20000"); // ~20MB page cache
        apply_migrations(&conn)?;
        Ok(Self {
            conn,
            path: path.to_path_buf(),
        })
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn ensure_project(&self, id: &str, name: &str, base_path: Option<&Path>) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT OR IGNORE INTO projects(id, name, base_path, settings_json, created_at, updated_at) VALUES(?1, ?2, ?3, '{}', ?4, ?4)",
            params![id, name, base_path.map(|p| p.to_string_lossy()), now],
        )?;
        Ok(())
    }

    pub fn set_project_base_path(&self, id: &str, base_path: &Path) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "UPDATE projects SET base_path = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, base_path.to_string_lossy(), now],
        )?;
        Ok(())
    }

    pub fn get_project_base_path(&self, id: &str) -> Result<Option<PathBuf>> {
        let mut stmt = self
            .conn
            .prepare("SELECT base_path FROM projects WHERE id = ?1 LIMIT 1")?;
        let mut rows = stmt.query(params![id])?;
        if let Some(row) = rows.next()? {
            let bp: Option<String> = row.get(0)?;
            Ok(bp.map(|s| PathBuf::from(s)))
        } else {
            Ok(None)
        }
    }

    pub fn delete_project(&self, project_id: &str) -> Result<ProjectDeleteResult> {
        let base_path = self.get_project_base_path(project_id)?;

        let mut proxy_set: HashSet<PathBuf> = HashSet::new();

        let mut tx = self.begin_tx()?;

        let mut asset_ids: Vec<String> = Vec::new();
        {
            let mut stmt = tx.prepare("SELECT id, proxy_path FROM assets WHERE project_id = ?1")?;
            let rows = stmt.query_map(params![project_id], |row| {
                let id: String = row.get(0)?;
                let proxy_path: Option<String> = row.get(1)?;
                Ok((id, proxy_path))
            })?;
            for row in rows {
                let (id, proxy_path) = row?;
                if let Some(path) = proxy_path {
                    proxy_set.insert(PathBuf::from(path));
                }
                asset_ids.push(id);
            }
        }

        {
            let mut stmt = tx.prepare("SELECT proxy_path FROM proxy_jobs WHERE project_id = ?1")?;
            let rows = stmt.query_map(params![project_id], |row| {
                let proxy_path: String = row.get(0)?;
                Ok(proxy_path)
            })?;
            for row in rows {
                let path = row?;
                proxy_set.insert(PathBuf::from(path));
            }
        }

        {
            let mut stmt = tx.prepare("SELECT path_abs FROM proxies WHERE asset_id IN (SELECT id FROM assets WHERE project_id = ?1)")?;
            let rows = stmt.query_map(params![project_id], |row| {
                let path: String = row.get(0)?;
                Ok(path)
            })?;
            for row in rows {
                let path = row?;
                proxy_set.insert(PathBuf::from(path));
            }
        }

        for asset_id in &asset_ids {
            tx.execute(
                "DELETE FROM asset_files WHERE asset_id = ?1",
                params![asset_id],
            )?;
            tx.execute("DELETE FROM proxies WHERE asset_id = ?1", params![asset_id])?;
            tx.execute("DELETE FROM cache WHERE asset_id = ?1", params![asset_id])?;
            tx.execute("DELETE FROM jobs WHERE asset_id = ?1", params![asset_id])?;
            tx.execute(
                "DELETE FROM asset_transcripts WHERE asset_id = ?1",
                params![asset_id],
            )?;
            tx.execute(
                "DELETE FROM proxy_jobs WHERE asset_id = ?1",
                params![asset_id],
            )?;
        }

        tx.execute(
            "DELETE FROM usages WHERE project_id = ?1",
            params![project_id],
        )?;
        tx.execute(
            "DELETE FROM proxy_jobs WHERE project_id = ?1",
            params![project_id],
        )?;
        tx.execute(
            "DELETE FROM project_timeline WHERE project_id = ?1",
            params![project_id],
        )?;
        tx.execute(
            "DELETE FROM sequences WHERE project_id = ?1",
            params![project_id],
        )?;
        tx.execute(
            "DELETE FROM assets WHERE project_id = ?1",
            params![project_id],
        )?;
        tx.execute("DELETE FROM projects WHERE id = ?1", params![project_id])?;

        tx.commit()?;

        Ok(ProjectDeleteResult {
            base_path,
            proxy_paths: proxy_set.into_iter().collect(),
        })
    }

    pub fn insert_asset_row(
        &self,
        project_id: &str,
        kind: &str,
        src_abs: &Path,
        src_rel: Option<&Path>,
        width: Option<i64>,
        height: Option<i64>,
        duration_frames: Option<i64>,
        fps_num: Option<i64>,
        fps_den: Option<i64>,
        audio_channels: Option<i64>,
        sample_rate: Option<i64>,
        duration_seconds: Option<f64>,
        codec: Option<&str>,
        bitrate_mbps: Option<f64>,
        bit_depth: Option<i64>,
        is_hdr: bool,
        is_variable_framerate: bool,
        metadata_json: Option<&str>,
    ) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        let now = chrono::Utc::now().timestamp();
        let meta = std::fs::metadata(src_abs).ok();
        let size = meta.as_ref().and_then(|m| Some(m.len() as i64));
        let mtime_ns = meta
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64);
        self.conn.execute(
            "INSERT OR REPLACE INTO assets(id, project_id, kind, src_abs, src_rel, referenced, file_size, mtime_ns, width, height, duration_frames, fps_num, fps_den, audio_channels, sample_rate, duration_seconds, codec, bitrate_mbps, proxy_path, is_proxy_ready, metadata_json, bit_depth, is_hdr, is_variable_framerate, created_at, updated_at) VALUES(?1, ?2, ?3, ?4, ?5, 1, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25)",
            params![
                id,
                project_id,
                kind,
                src_abs.to_string_lossy(),
                src_rel.map(|p| p.to_string_lossy()),
                size,
                mtime_ns,
                width,
                height,
                duration_frames,
                fps_num,
                fps_den,
                audio_channels,
                sample_rate,
                duration_seconds,
                codec,
                bitrate_mbps,
                Option::<String>::None,
                0,
                metadata_json.unwrap_or("null"),
                bit_depth,
                if is_hdr { 1 } else { 0 },
                if is_variable_framerate { 1 } else { 0 },
                now,
                now,
            ],
        )?;
        Ok(id)
    }

    pub fn list_asset_labels(&self, project_id: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT kind, src_abs, width, height FROM assets WHERE project_id = ?1 ORDER BY created_at DESC LIMIT 500",
        )?;
        let rows = stmt.query_map(params![project_id], |row| {
            let kind: String = row.get(0)?;
            let src_abs: String = row.get(1)?;
            let width: Option<i64> = row.get(2)?;
            let height: Option<i64> = row.get(3)?;
            let name = std::path::Path::new(&src_abs)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| src_abs.clone());
            let wh = match (width, height) {
                (Some(w), Some(h)) => format!(" {}x{}", w, h),
                _ => String::new(),
            };
            Ok(format!("[{}] {}{}", kind, name, wh))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn list_assets(&self, project_id: &str) -> Result<Vec<AssetRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, kind, src_abs, width, height, duration_frames, fps_num, fps_den, audio_channels, sample_rate, metadata_json, proxy_path, duration_seconds, codec, bitrate_mbps, is_proxy_ready, bit_depth, is_hdr, is_variable_framerate \
             FROM assets WHERE project_id = ?1 ORDER BY created_at DESC LIMIT 1000",
        )?;
        let rows = stmt.query_map(params![project_id], |row| {
            Ok(AssetRow {
                id: row.get(0)?,
                project_id: row.get(1)?,
                kind: row.get(2)?,
                src_abs: row.get(3)?,
                width: row.get(4)?,
                height: row.get(5)?,
                duration_frames: row.get(6)?,
                fps_num: row.get(7)?,
                fps_den: row.get(8)?,
                audio_channels: row.get(9)?,
                sample_rate: row.get(10)?,
                metadata_json: row.get(11)?,
                proxy_path: row.get(12)?,
                duration_seconds: row.get(13)?,
                codec: row.get(14)?,
                bitrate_mbps: row.get(15)?,
                is_proxy_ready: row.get::<_, Option<i64>>(16)?.unwrap_or(0) != 0,
                bit_depth: row.get(17)?,
                is_hdr: row.get::<_, Option<i64>>(18)?.unwrap_or(0) != 0,
                is_variable_framerate: row.get::<_, Option<i64>>(19)?.unwrap_or(0) != 0,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn list_projects(&self) -> Result<Vec<ProjectInfo>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, base_path FROM projects ORDER BY updated_at DESC, created_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ProjectInfo {
                id: row.get(0)?,
                name: row.get(1)?,
                base_path: row.get::<_, Option<String>>(2)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn find_asset_by_path(&self, project_id: &str, src_abs: &str) -> Result<Option<AssetRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, kind, src_abs, width, height, duration_frames, fps_num, fps_den, audio_channels, sample_rate, metadata_json, proxy_path, duration_seconds, codec, bitrate_mbps, is_proxy_ready, bit_depth, is_hdr, is_variable_framerate \
             FROM assets WHERE project_id = ?1 AND src_abs = ?2 LIMIT 1",
        )?;
        let mut rows = stmt.query(params![project_id, src_abs])?;
        if let Some(row) = rows.next()? {
            Ok(Some(AssetRow {
                id: row.get(0)?,
                project_id: row.get(1)?,
                kind: row.get(2)?,
                src_abs: row.get(3)?,
                width: row.get(4)?,
                height: row.get(5)?,
                duration_frames: row.get(6)?,
                fps_num: row.get(7)?,
                fps_den: row.get(8)?,
                audio_channels: row.get(9)?,
                sample_rate: row.get(10)?,
                metadata_json: row.get(11)?,
                proxy_path: row.get(12)?,
                duration_seconds: row.get(13)?,
                codec: row.get(14)?,
                bitrate_mbps: row.get(15)?,
                is_proxy_ready: row.get::<_, Option<i64>>(16)?.unwrap_or(0) != 0,
                bit_depth: row.get(17)?,
                is_hdr: row.get::<_, Option<i64>>(18)?.unwrap_or(0) != 0,
                is_variable_framerate: row.get::<_, Option<i64>>(19)?.unwrap_or(0) != 0,
            }))
        } else {
            Ok(None)
        }
    }
}

#[derive(Debug, Clone)]
pub struct AssetRow {
    pub id: String,
    pub project_id: String,
    pub kind: String,
    pub src_abs: String,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub duration_frames: Option<i64>,
    pub fps_num: Option<i64>,
    pub fps_den: Option<i64>,
    pub audio_channels: Option<i64>,
    pub sample_rate: Option<i64>,
    pub metadata_json: Option<String>,
    pub proxy_path: Option<String>,
    pub duration_seconds: Option<f64>,
    pub codec: Option<String>,
    pub bitrate_mbps: Option<f64>,
    pub is_proxy_ready: bool,
    pub bit_depth: Option<i64>,
    pub is_hdr: bool,
    pub is_variable_framerate: bool,
}

impl AssetRow {
    pub fn duration_seconds(&self) -> Option<f64> {
        if let Some(sec) = self.duration_seconds {
            if sec > 0.0 {
                return Some(sec);
            }
        }
        let frames = self.duration_frames?;
        let fps_num = self.fps_num?;
        let fps_den = self.fps_den?;
        if frames <= 0 || fps_num <= 0 || fps_den <= 0 {
            return None;
        }
        let frames = frames as f64;
        let fps_num = fps_num as f64;
        let fps_den = fps_den as f64;
        Some(frames * (fps_den / fps_num))
    }

    pub fn has_audio(&self) -> bool {
        self.audio_channels.unwrap_or(0) > 0
    }
}

#[derive(Debug, Clone, Default)]
pub struct AssetMediaDetails<'a> {
    pub duration_seconds: Option<f64>,
    pub codec: Option<&'a str>,
    pub bitrate_mbps: Option<f64>,
    pub proxy_path: Option<&'a std::path::Path>,
    pub is_proxy_ready: Option<bool>,
    pub bit_depth: Option<u32>,
    pub is_hdr: Option<bool>,
    pub is_variable_framerate: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct ProjectInfo {
    pub id: String,
    pub name: String,
    pub base_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AssetTranscriptRow {
    pub asset_id: String,
    pub project_id: String,
    pub checksum: Option<String>,
    pub json: String,
    pub source: Option<String>,
    pub version: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct JobRow {
    pub id: String,
    pub asset_id: String,
    pub kind: String,
    pub priority: i32,
}

#[derive(Debug, Clone)]
pub struct ProxyJobRow {
    pub id: String,
    pub project_id: String,
    pub asset_id: String,
    pub original_path: String,
    pub proxy_path: String,
    pub preset: String,
    pub reason: Option<String>,
    pub status: String,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub bitrate_kbps: Option<i64>,
    pub progress: f64,
    pub created_at: i64,
    pub updated_at: i64,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
}

pub struct ProxyJobInsert<'a> {
    pub id: &'a str,
    pub project_id: &'a str,
    pub asset_id: &'a str,
    pub original_path: &'a std::path::Path,
    pub proxy_path: &'a std::path::Path,
    pub preset: &'a str,
    pub reason: Option<&'a str>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub bitrate_kbps: Option<i64>,
}

impl ProjectDb {
    pub fn get_asset(&self, asset_id: &str) -> Result<AssetRow> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, kind, src_abs, width, height, duration_frames, fps_num, fps_den, audio_channels, sample_rate, metadata_json, proxy_path, duration_seconds, codec, bitrate_mbps, is_proxy_ready, bit_depth, is_hdr, is_variable_framerate \
             FROM assets WHERE id = ?1 LIMIT 1",
        )?;
        let mut rows = stmt.query(params![asset_id])?;
        if let Some(row) = rows.next()? {
            Ok(AssetRow {
                id: row.get(0)?,
                project_id: row.get(1)?,
                kind: row.get(2)?,
                src_abs: row.get(3)?,
                width: row.get(4)?,
                height: row.get(5)?,
                duration_frames: row.get(6)?,
                fps_num: row.get(7)?,
                fps_den: row.get(8)?,
                audio_channels: row.get(9)?,
                sample_rate: row.get(10)?,
                metadata_json: row.get(11)?,
                proxy_path: row.get(12)?,
                duration_seconds: row.get(13)?,
                codec: row.get(14)?,
                bitrate_mbps: row.get(15)?,
                is_proxy_ready: row.get::<_, Option<i64>>(16)?.unwrap_or(0) != 0,
                bit_depth: row.get(17)?,
                is_hdr: row.get::<_, Option<i64>>(18)?.unwrap_or(0) != 0,
                is_variable_framerate: row.get::<_, Option<i64>>(19)?.unwrap_or(0) != 0,
            })
        } else {
            Err(anyhow::anyhow!("asset not found"))
        }
    }

    pub fn upsert_transcript(
        &self,
        asset_id: &str,
        project_id: &str,
        json: &str,
        checksum: Option<&str>,
        source: Option<&str>,
        version: i64,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT INTO asset_transcripts(asset_id, project_id, checksum, json, source, version, created_at, updated_at) \
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7) \
             ON CONFLICT(asset_id) DO UPDATE SET project_id = excluded.project_id, checksum = excluded.checksum, json = excluded.json, source = excluded.source, version = excluded.version, updated_at = excluded.updated_at",
            params![asset_id, project_id, checksum, json, source, version, now],
        )?;
        Ok(())
    }

    pub fn get_transcript(&self, asset_id: &str) -> Result<Option<AssetTranscriptRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT asset_id, project_id, checksum, json, source, version, created_at, updated_at FROM asset_transcripts WHERE asset_id = ?1 LIMIT 1",
        )?;
        let mut rows = stmt.query(params![asset_id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(AssetTranscriptRow {
                asset_id: row.get(0)?,
                project_id: row.get(1)?,
                checksum: row.get(2)?,
                json: row.get(3)?,
                source: row.get(4)?,
                version: row.get(5)?,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn list_transcripts_for_project(
        &self,
        project_id: &str,
    ) -> Result<Vec<AssetTranscriptRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT asset_id, project_id, checksum, json, source, version, created_at, updated_at FROM asset_transcripts WHERE project_id = ?1",
        )?;
        let rows = stmt.query_map(params![project_id], |row| {
            Ok(AssetTranscriptRow {
                asset_id: row.get(0)?,
                project_id: row.get(1)?,
                checksum: row.get(2)?,
                json: row.get(3)?,
                source: row.get(4)?,
                version: row.get(5)?,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn delete_transcript(&self, asset_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM asset_transcripts WHERE asset_id = ?1",
            params![asset_id],
        )?;
        Ok(())
    }

    pub fn reset_running_jobs(&self) -> Result<()> {
        self.conn.execute(
            "UPDATE jobs SET status='pending' WHERE status='running'",
            [],
        )?;
        Ok(())
    }

    pub fn list_pending_jobs(&self) -> Result<Vec<JobRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, asset_id, kind, priority FROM jobs WHERE status = 'pending' ORDER BY priority DESC, created_at ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(JobRow {
                id: row.get(0)?,
                asset_id: row.get(1)?,
                kind: row.get(2)?,
                priority: row.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn insert_proxy_job(&self, job: &ProxyJobInsert<'_>) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT INTO proxy_jobs(id, project_id, asset_id, original_path, proxy_path, preset, reason, status, width, height, bitrate_kbps, progress, created_at, updated_at) \
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', ?8, ?9, ?10, 0.0, ?11, ?11)",
            params![
                job.id,
                job.project_id,
                job.asset_id,
                job.original_path.to_string_lossy(),
                job.proxy_path.to_string_lossy(),
                job.preset,
                job.reason,
                job.width,
                job.height,
                job.bitrate_kbps,
                now
            ],
        )?;
        Ok(())
    }

    pub fn update_proxy_job_status(
        &self,
        id: &str,
        status: &str,
        progress: Option<f64>,
        error: Option<&str>,
        started_at: Option<i64>,
        completed_at: Option<i64>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE proxy_jobs SET status = ?2, progress = COALESCE(?3, progress), error = COALESCE(?4, error), started_at = COALESCE(?5, started_at), completed_at = COALESCE(?6, completed_at), updated_at = strftime('%s','now') WHERE id = ?1",
            params![id, status, progress, error, started_at, completed_at],
        )?;
        Ok(())
    }

    pub fn list_proxy_jobs_by_status(&self, status: &str) -> Result<Vec<ProxyJobRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, asset_id, original_path, proxy_path, preset, reason, status, width, height, bitrate_kbps, progress, created_at, updated_at, started_at, completed_at \
             FROM proxy_jobs WHERE status = ?1 ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map(params![status], |row| {
            let progress: Option<f64> = row.get(11)?;
            Ok(ProxyJobRow {
                id: row.get(0)?,
                project_id: row.get(1)?,
                asset_id: row.get(2)?,
                original_path: row.get(3)?,
                proxy_path: row.get(4)?,
                preset: row.get(5)?,
                reason: row.get(6)?,
                status: row.get(7)?,
                width: row.get(8)?,
                height: row.get(9)?,
                bitrate_kbps: row.get(10)?,
                progress: progress.unwrap_or(0.0),
                created_at: row.get(12)?,
                updated_at: row.get(13)?,
                started_at: row.get(14)?,
                completed_at: row.get(15)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn find_proxy_job_for_asset(&self, asset_id: &str) -> Result<Option<ProxyJobRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, asset_id, original_path, proxy_path, preset, reason, status, width, height, bitrate_kbps, progress, created_at, updated_at, started_at, completed_at \
             FROM proxy_jobs WHERE asset_id = ?1 AND status IN ('pending','running') LIMIT 1",
        )?;
        let mut rows = stmt.query(params![asset_id])?;
        if let Some(row) = rows.next()? {
            let progress: Option<f64> = row.get(11)?;
            Ok(Some(ProxyJobRow {
                id: row.get(0)?,
                project_id: row.get(1)?,
                asset_id: row.get(2)?,
                original_path: row.get(3)?,
                proxy_path: row.get(4)?,
                preset: row.get(5)?,
                reason: row.get(6)?,
                status: row.get(7)?,
                width: row.get(8)?,
                height: row.get(9)?,
                bitrate_kbps: row.get(10)?,
                progress: progress.unwrap_or(0.0),
                created_at: row.get(12)?,
                updated_at: row.get(13)?,
                started_at: row.get(14)?,
                completed_at: row.get(15)?,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn find_latest_proxy_job_for_asset(&self, asset_id: &str) -> Result<Option<ProxyJobRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, asset_id, original_path, proxy_path, preset, reason, status, width, height, bitrate_kbps, progress, created_at, updated_at, started_at, completed_at \
             FROM proxy_jobs WHERE asset_id = ?1 ORDER BY updated_at DESC LIMIT 1",
        )?;
        let mut rows = stmt.query(params![asset_id])?;
        if let Some(row) = rows.next()? {
            let progress: Option<f64> = row.get(11)?;
            Ok(Some(ProxyJobRow {
                id: row.get(0)?,
                project_id: row.get(1)?,
                asset_id: row.get(2)?,
                original_path: row.get(3)?,
                proxy_path: row.get(4)?,
                preset: row.get(5)?,
                reason: row.get(6)?,
                status: row.get(7)?,
                width: row.get(8)?,
                height: row.get(9)?,
                bitrate_kbps: row.get(10)?,
                progress: progress.unwrap_or(0.0),
                created_at: row.get(12)?,
                updated_at: row.get(13)?,
                started_at: row.get(14)?,
                completed_at: row.get(15)?,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn update_asset_media_details(
        &self,
        asset_id: &str,
        details: &AssetMediaDetails<'_>,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        let ready_val = details.is_proxy_ready.map(|v| if v { 1 } else { 0 });
        let hdr_val = details.is_hdr.map(|v| if v { 1 } else { 0 });
        let vfr_val = details.is_variable_framerate.map(|v| if v { 1 } else { 0 });
        self.conn.execute(
            "UPDATE assets SET duration_seconds = COALESCE(?2, duration_seconds), codec = COALESCE(?3, codec), bitrate_mbps = COALESCE(?4, bitrate_mbps), proxy_path = COALESCE(?5, proxy_path), is_proxy_ready = CASE WHEN ?6 IS NULL THEN is_proxy_ready ELSE ?6 END, bit_depth = COALESCE(?7, bit_depth), is_hdr = CASE WHEN ?8 IS NULL THEN is_hdr ELSE ?8 END, is_variable_framerate = CASE WHEN ?9 IS NULL THEN is_variable_framerate ELSE ?9 END, updated_at = ?10 WHERE id = ?1",
            params![
                asset_id,
                details.duration_seconds,
                details.codec,
                details.bitrate_mbps,
                details
                    .proxy_path
                    .map(|p| p.to_string_lossy().to_string()),
                ready_val,
                details.bit_depth.map(|v| v as i64),
                hdr_val,
                vfr_val,
                now,
            ],
        )?;
        Ok(())
    }
}

fn ensure_column(conn: &Connection, table: &str, column: &str, alter_sql: &str) -> Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    let mut exists = false;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name.eq_ignore_ascii_case(column) {
            exists = true;
            break;
        }
    }
    if !exists {
        conn.execute(alter_sql, [])?;
    }
    Ok(())
}

fn apply_migrations(conn: &Connection) -> Result<()> {
    // Simple migration tracking by name
    conn.execute_batch(include_str!("../migrations/V0001__init.sql"))?;
    conn.execute(
        "INSERT OR IGNORE INTO migrations(name, applied_at) VALUES(?1, strftime('%s','now'))",
        params!["V0001__init"],
    )?;
    // Jobs & status (V0002)
    conn.execute_batch(include_str!("../migrations/V0002__jobs.sql"))?;
    conn.execute(
        "INSERT OR IGNORE INTO migrations(name, applied_at) VALUES(?1, strftime('%s','now'))",
        params!["V0002__jobs"],
    )?;
    // Project timeline (V0003)
    conn.execute_batch(include_str!("../migrations/V0003__timeline.sql"))?;
    conn.execute(
        "INSERT OR IGNORE INTO migrations(name, applied_at) VALUES(?1, strftime('%s','now'))",
        params!["V0003__timeline"],
    )?;
    // Asset transcripts (V0004)
    conn.execute_batch(include_str!("../migrations/V0004__transcripts.sql"))?;
    conn.execute(
        "INSERT OR IGNORE INTO migrations(name, applied_at) VALUES(?1, strftime('%s','now'))",
        params!["V0004__transcripts"],
    )?;
    ensure_column(
        conn,
        "assets",
        "metadata_json",
        "ALTER TABLE assets ADD COLUMN metadata_json TEXT",
    )?;
    ensure_column(
        conn,
        "assets",
        "proxy_path",
        "ALTER TABLE assets ADD COLUMN proxy_path TEXT",
    )?;
    ensure_column(
        conn,
        "assets",
        "duration_seconds",
        "ALTER TABLE assets ADD COLUMN duration_seconds REAL",
    )?;
    ensure_column(
        conn,
        "assets",
        "codec",
        "ALTER TABLE assets ADD COLUMN codec TEXT",
    )?;
    ensure_column(
        conn,
        "assets",
        "bitrate_mbps",
        "ALTER TABLE assets ADD COLUMN bitrate_mbps REAL",
    )?;
    ensure_column(
        conn,
        "assets",
        "is_proxy_ready",
        "ALTER TABLE assets ADD COLUMN is_proxy_ready INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(
        conn,
        "assets",
        "bit_depth",
        "ALTER TABLE assets ADD COLUMN bit_depth INTEGER",
    )?;
    ensure_column(
        conn,
        "assets",
        "is_hdr",
        "ALTER TABLE assets ADD COLUMN is_hdr INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(
        conn,
        "assets",
        "is_variable_framerate",
        "ALTER TABLE assets ADD COLUMN is_variable_framerate INTEGER NOT NULL DEFAULT 0",
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO migrations(name, applied_at) VALUES(?1, strftime('%s','now'))",
        params!["V0005__asset_metadata"],
    )?;
    conn.execute_batch(include_str!("../migrations/V0006__proxy_jobs.sql"))?;
    conn.execute(
        "INSERT OR IGNORE INTO migrations(name, applied_at) VALUES(?1, strftime('%s','now'))",
        params!["V0006__proxy_jobs"],
    )?;
    Ok(())
}

impl ProjectDb {
    pub fn get_project_timeline_json(&self, project_id: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT json FROM project_timeline WHERE project_id = ?1 LIMIT 1")?;
        let mut rows = stmt.query(params![project_id])?;
        if let Some(row) = rows.next()? {
            let json: String = row.get(0)?;
            Ok(Some(json))
        } else {
            Ok(None)
        }
    }

    pub fn upsert_project_timeline_json(&self, project_id: &str, json: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT INTO project_timeline(project_id, json, updated_at) VALUES(?1, ?2, ?3)
             ON CONFLICT(project_id) DO UPDATE SET json = excluded.json, updated_at = excluded.updated_at",
            params![project_id, json, now],
        )?;
        Ok(())
    }
}
