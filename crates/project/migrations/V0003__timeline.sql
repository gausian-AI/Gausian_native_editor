-- Project timeline storage
CREATE TABLE IF NOT EXISTS project_timeline (
  project_id TEXT PRIMARY KEY,
  json TEXT NOT NULL,
  updated_at INTEGER NOT NULL
);
