//! Work-item persistence in `work-items.json`, separate from the session snapshot.

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use serde::{Deserialize, Serialize};
use tracing::warn;

use super::state::WorkItem;

const STORE_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    items: Vec<WorkItem>,
}

#[derive(Serialize)]
struct StoreFileRef<'a> {
    version: u32,
    items: &'a [WorkItem],
}

pub(crate) fn load(path: &Path) -> Vec<WorkItem> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            warn!(path = %path.display(), err = %err, "failed to read work items; starting empty");
            return Vec::new();
        }
    };
    match serde_json::from_str::<StoreFile>(&contents) {
        Ok(file) if file.version == STORE_VERSION => file.items,
        Ok(file) => {
            warn!(
                path = %path.display(),
                version = file.version,
                "unsupported work items version; starting empty"
            );
            Vec::new()
        }
        Err(err) => {
            warn!(path = %path.display(), err = %err, "invalid work items file; starting empty");
            Vec::new()
        }
    }
}

/// Writes the latest serialised state on one background thread.
#[derive(Debug)]
pub(crate) struct StoreWriter {
    tx: mpsc::Sender<String>,
}

impl StoreWriter {
    pub(crate) fn spawn(path: PathBuf) -> Option<Self> {
        let (tx, rx) = mpsc::channel::<String>();
        let spawned = std::thread::Builder::new()
            .name("herdr-work-items-store".into())
            .spawn(move || {
                while let Ok(mut latest) = rx.recv() {
                    while let Ok(newer) = rx.try_recv() {
                        latest = newer;
                    }
                    if let Err(err) = write_atomically(&path, &latest) {
                        warn!(path = %path.display(), err = %err, "failed to save work items");
                    }
                }
            });
        match spawned {
            Ok(_) => Some(Self { tx }),
            Err(err) => {
                warn!(err = %err, "failed to start work items store writer");
                None
            }
        }
    }

    pub(crate) fn save(&self, items: &[WorkItem]) {
        match serde_json::to_string(&StoreFileRef {
            version: STORE_VERSION,
            items,
        }) {
            Ok(json) => {
                let _ = self.tx.send(json);
            }
            Err(err) => warn!(err = %err, "failed to serialise work items"),
        }
    }
}

fn write_atomically(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{WorkItemPhase, WorkItemProvisioningInfo};

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "herdr-work-items-store-{}-{name}",
                std::process::id()
            ))
            .join("work-items.json")
    }

    fn item() -> WorkItem {
        WorkItem {
            key: "gh:o/r#1".into(),
            source_id: "gh".into(),
            external_id: "o/r#1".into(),
            title: "Title".into(),
            context: "o/r #1".into(),
            author: Some("octocat".into()),
            url: "https://example.test".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            detail: Some(serde_json::json!({"number": 1})),
            summary: Some("+1 −0".into()),
            prepare_error: None,
            phase: WorkItemPhase::Local,
            seen: true,
            resolved: false,
            workspace_id: Some("w1".into()),
            prepared_for: Some("2026-01-01T00:00:00Z".into()),
            prepare_in_flight: true,
            provisioning: Some(WorkItemProvisioningInfo {
                steps: Vec::new(),
                finished: false,
            }),
        }
    }

    #[test]
    fn round_trip_keeps_persisted_fields_and_drops_transient_ones() {
        let path = temp_path("round-trip");
        let original = item();
        write_atomically(
            &path,
            &serde_json::to_string(&StoreFileRef {
                version: STORE_VERSION,
                items: std::slice::from_ref(&original),
            })
            .expect("serialises"),
        )
        .expect("writes");
        let loaded = load(&path);
        let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
        let expected = WorkItem {
            prepared_for: None,
            prepare_in_flight: false,
            provisioning: None,
            ..original
        };
        assert_eq!(loaded, vec![expected]);
    }

    #[test]
    fn newer_version_loads_empty() {
        let path = temp_path("newer");
        write_atomically(
            &path,
            &serde_json::to_string(&StoreFileRef {
                version: STORE_VERSION + 1,
                items: &[item()],
            })
            .expect("serialises"),
        )
        .expect("writes");
        let loaded = load(&path);
        let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
        assert!(loaded.is_empty());
    }
}
