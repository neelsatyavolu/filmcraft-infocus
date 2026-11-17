//! Split a project into shared and per-sequence parts, and merge them back.

use std::collections::BTreeMap;
use std::path::PathBuf;

use filmcraft_project::{ItemId, ItemKind, MediaRef, Project, ProjectItem};
use serde_json::Value;

/// Marks a media item in the shared part whose file is not on the Drive: only its file name is
/// kept, and no other Mac can resolve it.
const LOCAL_ONLY: &str = "local_only";

/// The prefix of a team media path (`drive:/<path on the Drive>`).
pub const DRIVE_PREFIX: &str = "drive:/";

/// A team project as stored on the Drive: one shared part and one part per sequence.
#[derive(Clone, Debug, PartialEq)]
pub struct TeamDoc {
    /// The project without its sequence items: settings, bins, media and other items,
    /// transcripts, source graphics, narrations and notes. Proxies and local paths are stripped.
    pub shared: Value,
    /// Each sequence item (a serialized `ProjectItem`), by id.
    pub sequences: BTreeMap<ItemId, Value>,
}

/// This Mac's copies of one team media item, found by its `drive:/` path.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocalMedia {
    /// The downloaded original, when this Mac has it.
    pub original: Option<PathBuf>,
    /// This Mac's proxy, when it has one.
    pub proxy: Option<PathBuf>,
}

/// Why a team part could not be read or applied.
#[derive(Debug, thiserror::Error)]
pub enum TeamError {
    #[error("team project part is damaged: {0}")]
    Corrupt(String),
    #[error("item {0} is not a sequence")]
    NotASequence(u64),
    #[error("sequence part for {expected} holds item {found}")]
    WrongId { expected: u64, found: u64 },
    #[error(transparent)]
    Format(#[from] filmcraft_format::FormatError),
    #[error("could not serialize the project: {0}")]
    Serialize(String),
}

/// Split `p` into a shared part and one part per sequence. Proxies and this Mac's scratch and
/// ingest folders are dropped. Each file path becomes the Drive path `media_ref` returns; a file
/// that is not on the Drive keeps only its file name and is marked `"local_only": true`.
pub fn split(p: &Project, media_ref: &dyn Fn(&MediaRef) -> Option<String>) -> Result<TeamDoc, TeamError> {
    let mut shared = p.clone();
    shared.settings.scratch = Default::default();
    shared.settings.ingest.destination = None;
    let mut local_only = Vec::new();
    let mut sequences = BTreeMap::new();
    let ids: Vec<ItemId> = shared.items.keys().copied().collect();
    for id in ids {
        let Some(item) = shared.items.get_mut(&id) else { continue };
        match &mut item.kind {
            ItemKind::Sequence(_) => {
                if let Some(item) = shared.items.remove(&id) {
                    sequences.insert(id, serde_json::to_value(&item).map_err(|e| TeamError::Serialize(e.to_string()))?);
                }
            }
            ItemKind::Media(m) => {
                m.proxy = None;
                if let MediaRef::File { path } = &m.media {
                    match media_ref(&m.media) {
                        Some(drive) => m.media = MediaRef::File { path: drive },
                        None => {
                            m.media = MediaRef::File { path: file_name(path).to_owned() };
                            local_only.push(id);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let mut shared = serde_json::to_value(&shared).map_err(|e| TeamError::Serialize(e.to_string()))?;
    for id in local_only {
        if let Some(Value::Object(item)) = shared.get_mut("items").and_then(|items| items.get_mut(id.0.to_string())) {
            item.insert(LOCAL_ONLY.into(), Value::Bool(true));
        }
    }
    Ok(TeamDoc { shared, sequences })
}

/// Rebuild a project from its parts, filling in this Mac's originals and proxies for `drive:/`
/// paths with `resolve`. The result is checked like a project file read from disk, so a damaged
/// or hostile part is refused, not loaded. Its `id_slot` is 0: call `Project::set_id_slot` next.
pub fn merge(doc: &TeamDoc, resolve: &dyn Fn(&str) -> LocalMedia) -> Result<Project, TeamError> {
    if !doc.shared.get("items").is_some_and(Value::is_object) {
        return Err(TeamError::Corrupt("the shared part has no items".into()));
    }
    let mut shared = doc.shared.clone();
    if let Some(Value::Object(items)) = shared.get_mut("items") {
        for item in items.values_mut() {
            if let Value::Object(item) = item {
                item.remove(LOCAL_ONLY);
            }
        }
    }
    let mut p: Project = serde_json::from_value(shared).map_err(|e| TeamError::Corrupt(format!("shared part: {e}")))?;
    for (&id, json) in &doc.sequences {
        let item = sequence_item(id, json)?;
        p.items.insert(id, item);
    }
    let mut p = validated(&p)?;
    for item in p.items.values_mut() {
        let Some(m) = item.as_media_mut() else { continue };
        let MediaRef::File { path } = &m.media else { continue };
        if !path.starts_with(DRIVE_PREFIX) {
            continue;
        }
        let local = resolve(path);
        if let Some(original) = local.original {
            m.media = MediaRef::File { path: original.to_string_lossy().into_owned() };
        }
        m.proxy = local.proxy.map(|p| MediaRef::File { path: p.to_string_lossy().into_owned() });
    }
    Ok(p)
}

/// The sequence item `id` as JSON, or None when `id` is not a sequence.
pub fn sequence_json(p: &Project, id: ItemId) -> Option<Value> {
    let item = p.item(id)?;
    item.as_sequence()?;
    serde_json::to_value(item).ok()
}

/// Replace (or add) the sequence item `id` with `json`, a part received from a teammate. Nothing
/// else in `p` changes, and nothing changes at all when `json` is not a valid sequence `id` or
/// `id` is an item of another kind.
pub fn replace_sequence(p: &mut Project, id: ItemId, json: &Value) -> Result<(), TeamError> {
    if p.item(id).is_some_and(|item| item.as_sequence().is_none()) {
        return Err(TeamError::NotASequence(id.0));
    }
    let item = sequence_item(id, json)?;
    // Check it the way a project file is checked, on its own.
    let mut alone = Project::new("");
    alone.items.insert(id, item);
    let mut alone = validated(&alone)?;
    let item = alone.items.remove(&id).ok_or_else(|| TeamError::Corrupt(format!("sequence {} was dropped while checking it", id.0)))?;
    p.items.insert(id, item);
    Ok(())
}

/// Read a sequence part and check that it is the sequence `id`.
fn sequence_item(id: ItemId, json: &Value) -> Result<ProjectItem, TeamError> {
    let item: ProjectItem = serde_json::from_value(json.clone()).map_err(|e| TeamError::Corrupt(format!("sequence {}: {e}", id.0)))?;
    if item.id != id {
        return Err(TeamError::WrongId { expected: id.0, found: item.id.0 });
    }
    if item.as_sequence().is_none() {
        return Err(TeamError::NotASequence(id.0));
    }
    Ok(item)
}

/// `p` after the checks and repairs a project file gets when it is opened (`filmcraft_format`).
fn validated(p: &Project) -> Result<Project, TeamError> {
    Ok(filmcraft_format::decode(&filmcraft_format::encode(p, false))?.project)
}

/// The last component of a path, for either separator.
fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}
