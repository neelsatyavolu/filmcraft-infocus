//! Project-level fields of the shared part (settings, colours, LUTs, transcripts, search bins,
//! source graphics, narrations and notes). The Drive takes each one whole and keeps the last one
//! written (`POST …/shared` with `fields`). This Mac's scratch disks and ingest destination never
//! leave it.

use std::collections::{BTreeMap, BTreeSet};

use filmcraft_project::{Project, ProjectSettings};
use serde::Serialize;
use serde_json::Value;

use crate::doc::TeamError;

/// The project fields the Drive shares, by their JSON key (the server's allowlist).
pub const SHARED_FIELDS: [&str; 9] =
    ["settings", "custom_colors", "custom_color_names", "luts", "transcripts", "search_bins", "source_graphics", "narrations", "notes"];

/// The settings as shared: without this Mac's scratch disks and ingest destination.
fn shared_settings(s: &ProjectSettings) -> ProjectSettings {
    let mut s = s.clone();
    s.scratch = Default::default();
    s.ingest.destination = None;
    s
}

/// Equal values (`==`, then as JSON, because unset point parameters are NaN).
fn same<T: PartialEq + Serialize>(b: &T, a: &T) -> bool {
    b == a
        || match (serde_json::to_value(b), serde_json::to_value(a)) {
            (Ok(b), Ok(a)) => b == a,
            _ => false,
        }
}

/// The shared fields that differ between two snapshots (scratch disks and the ingest destination
/// do not count).
pub fn changed_fields(before: &Project, after: &Project) -> BTreeSet<String> {
    SHARED_FIELDS
        .iter()
        .filter(|key| {
            let equal = match **key {
                "settings" => before.settings == after.settings || same(&shared_settings(&before.settings), &shared_settings(&after.settings)),
                "custom_colors" => same(&before.custom_colors, &after.custom_colors),
                "custom_color_names" => same(&before.custom_color_names, &after.custom_color_names),
                "luts" => same(&before.luts, &after.luts),
                "transcripts" => same(&before.transcripts, &after.transcripts),
                "search_bins" => same(&before.search_bins, &after.search_bins),
                "source_graphics" => same(&before.source_graphics, &after.source_graphics),
                "narrations" => same(&before.narrations, &after.narrations),
                "notes" => before.notes == after.notes,
                _ => true,
            };
            !equal
        })
        .map(|key| key.to_string())
        .collect()
}

/// The JSON of shared fields `keys` of `p`, as the Drive takes them. Unknown keys are left out.
pub fn field_values(p: &Project, keys: &BTreeSet<String>) -> Result<BTreeMap<String, Value>, TeamError> {
    let json = |r: serde_json::Result<Value>| r.map_err(|e| TeamError::Serialize(e.to_string()));
    let mut out = BTreeMap::new();
    for key in keys {
        let v = match key.as_str() {
            "settings" => json(serde_json::to_value(shared_settings(&p.settings)))?,
            "custom_colors" => json(serde_json::to_value(&p.custom_colors))?,
            "custom_color_names" => json(serde_json::to_value(&p.custom_color_names))?,
            "luts" => json(serde_json::to_value(&p.luts))?,
            "transcripts" => json(serde_json::to_value(&p.transcripts))?,
            "search_bins" => json(serde_json::to_value(&p.search_bins))?,
            "source_graphics" => json(serde_json::to_value(&p.source_graphics))?,
            "narrations" => json(serde_json::to_value(&p.narrations))?,
            "notes" => Value::String(p.notes.clone()),
            _ => continue,
        };
        out.insert(key.clone(), v);
    }
    Ok(out)
}

/// Copy every shared field except `skip` from `from` (a teammate's version) into `p`. `p` keeps
/// its own scratch disks and ingest destination.
pub fn copy_fields(p: &mut Project, from: &Project, skip: &BTreeSet<String>) {
    let take = |key: &str| !skip.contains(key);
    if take("settings") {
        let mut s = from.settings.clone();
        s.scratch = p.settings.scratch.clone();
        s.ingest.destination = p.settings.ingest.destination.clone();
        p.settings = s;
    }
    if take("custom_colors") {
        p.custom_colors = from.custom_colors.clone();
    }
    if take("custom_color_names") {
        p.custom_color_names = from.custom_color_names.clone();
    }
    if take("luts") {
        p.luts = from.luts.clone();
    }
    if take("transcripts") {
        p.transcripts = from.transcripts.clone();
    }
    if take("search_bins") {
        p.search_bins = from.search_bins.clone();
    }
    if take("source_graphics") {
        p.source_graphics = from.source_graphics.clone();
    }
    if take("narrations") {
        p.narrations = from.narrations.clone();
    }
    if take("notes") {
        p.notes = from.notes.clone();
    }
}
