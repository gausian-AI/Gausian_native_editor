-- Add comfy metadata to assets
BEGIN IMMEDIATE;
ALTER TABLE assets ADD COLUMN IF NOT EXISTS metadata_json TEXT;
COMMIT;
