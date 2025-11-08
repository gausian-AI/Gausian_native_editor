use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
pub struct CacheJobId(pub u64);

#[derive(Clone, Debug)]
pub struct CacheJobSpec {
    pub source_path: PathBuf,
    pub force_container_mov: bool,
    pub preferred_codec: PreferredCodec,
    pub source_codec: Option<String>,
}

#[derive(Clone, Debug)]
pub enum PreferredCodec {
    ProRes422,
}

#[derive(Clone, Debug)]
pub enum JobStatus {
    Queued,
    InProgress(f32),
    Completed(PathBuf),
    Failed(String),
    Canceled,
}

#[derive(Clone, Debug)]
pub enum CacheEvent {
    StatusChanged { id: CacheJobId, status: JobStatus },
}
