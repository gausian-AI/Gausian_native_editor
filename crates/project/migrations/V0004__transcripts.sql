BEGIN IMMEDIATE;

CREATE TABLE IF NOT EXISTS asset_transcripts (
  asset_id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL,
  checksum TEXT,
  json TEXT NOT NULL,
  source TEXT,
  version INTEGER NOT NULL DEFAULT 1,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_asset_transcripts_project_id ON asset_transcripts(project_id);

COMMIT;
