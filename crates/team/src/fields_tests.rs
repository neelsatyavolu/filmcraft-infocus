use std::collections::BTreeSet;

use filmcraft_project::Project;
use serde_json::{Value, json};

use super::*;

fn keys(k: &[&str]) -> BTreeSet<String> {
    k.iter().map(|s| s.to_string()).collect()
}

#[test]
fn changed_fields_names_each_field_and_ignores_local_folders() {
    let before = Project::new("Robotics");
    let mut after = before.clone();
    assert!(changed_fields(&before, &after).is_empty());
    after.notes = "call the gym".into();
    after.settings.ingest.destination = Some("/Users/me/Ingest".into());
    assert_eq!(changed_fields(&before, &after), keys(&["notes"]), "the ingest folder stays on this Mac");
    after.settings.ingest.preset = "prores_proxy_half".into();
    assert_eq!(changed_fields(&before, &after), keys(&["notes", "settings"]));
}

#[test]
fn field_values_match_the_drive_allowlist_types() {
    let mut p = Project::new("Robotics");
    p.notes = "hello".into();
    p.settings.ingest.destination = Some("/Users/me/Ingest".into());
    let all: BTreeSet<String> = SHARED_FIELDS.iter().map(|s| s.to_string()).chain(["name".to_string()]).collect();
    let v = field_values(&p, &all).unwrap();
    assert_eq!(v.len(), SHARED_FIELDS.len(), "name is not a shared field");
    assert_eq!(v["notes"], json!("hello"));
    for (key, kind) in [("custom_colors", "array"), ("luts", "array"), ("search_bins", "array")] {
        assert!(v[key].is_array(), "{key} must be a JSON {kind}");
    }
    for key in ["settings", "custom_color_names", "transcripts", "source_graphics", "narrations"] {
        assert!(v[key].is_object(), "{key} must be a JSON object");
    }
    assert_eq!(v["settings"]["ingest"]["destination"], Value::Null, "the ingest folder is not sent");
}

#[test]
fn copy_fields_keeps_skipped_and_local_folders() {
    let mut mine = Project::new("Robotics");
    mine.settings.ingest.destination = Some("/Users/me/Ingest".into());
    mine.notes = "mine".into();
    let mut theirs = Project::new("Robotics");
    theirs.notes = "theirs".into();
    theirs.settings.ingest.preset = "prores_proxy_half".into();
    let mut a = mine.clone();
    copy_fields(&mut a, &theirs, &BTreeSet::new());
    assert_eq!(a.notes, "theirs");
    assert_eq!(a.settings.ingest.preset, "prores_proxy_half");
    assert_eq!(a.settings.ingest.destination.as_deref(), Some("/Users/me/Ingest"));
    let mut b = mine.clone();
    copy_fields(&mut b, &theirs, &keys(&["notes"]));
    assert_eq!(b.notes, "mine", "a field still on its way to the Drive keeps this Mac's value");
}
