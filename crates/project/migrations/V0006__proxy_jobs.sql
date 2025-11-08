BEGIN IMMEDIATE;

CREATE TABLE IF NOT EXISTS proxy_jobs (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL,
  asset_id TEXT NOT NULL,
  original_path TEXT NOT NULL,
  proxy_path TEXT NOT NULL,
  preset TEXT NOT NULL,
  reason TEXT,
  status TEXT NOT NULL DEFAULT 'pending',
  width INTEGER,
  height INTEGER,
  bitrate_kbps INTEGER,
  progress REAL NOT NULL DEFAULT 0.0,
  error TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  started_at INTEGER,
  completed_at INTEGER
);
CREATE INDEX IF NOT EXISTS idx_proxy_jobs_status ON proxy_jobs(status);
CREATE INDEX IF NOT EXISTS idx_proxy_jobs_asset ON proxy_jobs(asset_id);

COMMIT;
