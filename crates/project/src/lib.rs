use anyhow::Result;
use rusqlite::{params, Connection, Transaction};
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
            project_id, kind, src_abs, None, None, None, None, None, None, None, None,
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
        let mut stmt = self.conn.prepare("SELECT base_path FROM projects WHERE id = ?1 LIMIT 1")?;
        let mut rows = stmt.query(params![id])?;
        if let Some(row) = rows.next()? {
            let bp: Option<String> = row.get(0)?;
            Ok(bp.map(|s| PathBuf::from(s)))
        } else {
            Ok(None)
        }
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
            "INSERT OR REPLACE INTO assets(id, project_id, kind, src_abs, src_rel, referenced, file_size, mtime_ns, width, height, duration_frames, fps_num, fps_den, audio_channels, sample_rate, created_at, updated_at) VALUES(?1, ?2, ?3, ?4, ?5, 1, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?15)",
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
                now
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
            "SELECT id, project_id, kind, src_abs, width, height, duration_frames, fps_num, fps_den \
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
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn list_projects(&self) -> Result<Vec<ProjectInfo>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, base_path FROM projects ORDER BY updated_at DESC, created_at DESC")?;
        let rows = stmt.query_map([], |row| {
            Ok(ProjectInfo {
                id: row.get(0)?,
                name: row.get(1)?,
                base_path: row.get::<_, Option<String>>(2)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows { out.push(r?); }
        Ok(out)
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
}

#[derive(Debug, Clone)]
pub struct ProjectInfo {
    pub id: String,
    pub name: String,
    pub base_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct JobRow {
    pub id: String,
    pub asset_id: String,
    pub kind: String,
    pub priority: i32,
}

impl ProjectDb {
    pub fn get_asset(&self, asset_id: &str) -> Result<AssetRow> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, kind, src_abs, width, height, duration_frames, fps_num, fps_den \
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
            })
        } else {
            Err(anyhow::anyhow!("asset not found"))
        }
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
