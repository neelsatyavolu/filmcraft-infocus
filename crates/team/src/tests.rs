use std::collections::BTreeMap;
use std::path::PathBuf;

use filmcraft_media::{DemoScene, Generator, MediaSource};
use filmcraft_project::{ItemId, ItemKind, Label, MediaClip, MediaRef, Project, SequenceSettings, TrackKind};
use filmcraft_time::{TICKS_PER_SECOND, Tick, TimeRange};
use serde_json::Value;

use super::*;

const DRIVE_ROOT: &str = "/Volumes/Drive/";

fn media(path: &str) -> ItemKind {
    let info = filmcraft_media::generators::GeneratorSource::demo(DemoScene::OceanSunset).info().clone();
    ItemKind::Media(Box::new(MediaClip {
        media: MediaRef::File { path: path.into() },
        info,
        interpret: Default::default(),
        mark_in: None,
        mark_out: None,
        markers: vec![],
        offline: false,
        proxy: Some(MediaRef::File { path: format!("{path}.proxy.mov") }),
        identity: None,
    }))
}

/// Two sequences and three media items: two on the Drive, one only on this Mac.
fn sample() -> (Project, [ItemId; 3], [ItemId; 2]) {
    let mut p = Project::new("Robotics");
    let a = p.add_item("A001.mov", Label::Iris, media("/Volumes/Drive/Team/Media/A001.mov"), None);
    let b = p.add_item("A002.mov", Label::Iris, media("/Volumes/Drive/Team/Media/A002.mov"), None);
    let c = p.add_item("phone.mov", Label::Iris, media("/Users/someone/Desktop/phone.mov"), None);
    let s1 = p.new_sequence("Package", SequenceSettings::default(), 2, 2, None);
    let s2 = p.new_sequence("Teaser", SequenceSettings::default(), 2, 2, None);
    for (seq, item) in [(s1, a), (s1, c), (s2, b)] {
        let rate = p.sequence(seq).unwrap().settings.frame_rate;
        let ti = p.make_track_item(item, TrackKind::Video, Tick::ZERO, TimeRange::new(Tick::ZERO, Tick(2 * TICKS_PER_SECOND)), rate).unwrap();
        p.sequence_mut(seq).unwrap().video_tracks[0].items.push(ti);
    }
    p.settings.scratch.auto_save = Some("/Users/someone/AutoSave".into());
    (p, [a, b, c], [s1, s2])
}

fn drive_ref(m: &MediaRef) -> Option<String> {
    match m {
        MediaRef::File { path } => path.strip_prefix(DRIVE_ROOT).map(|rest| format!("drive:/{rest}")),
        MediaRef::Generator(_) => None,
    }
}

fn resolve_back(r: &str) -> LocalMedia {
    match r.strip_prefix("drive:/") {
        Some(rest) => LocalMedia { original: Some(PathBuf::from(format!("{DRIVE_ROOT}{rest}"))), proxy: None },
        None => LocalMedia { original: None, proxy: None },
    }
}

/// Projects compared as JSON: unset point parameters (frame centre) are NaN, so `==` on a project
/// with video clips is never true, even against its own clone.
fn as_json<T: serde::Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap()
}

fn media_path(p: &Project, id: ItemId) -> String {
    match &p.item(id).unwrap().as_media().unwrap().media {
        MediaRef::File { path } => path.clone(),
        MediaRef::Generator(_) => String::new(),
    }
}

#[test]
fn split_merge_round_trip() {
    let (p, [a, b, c], [s1, s2]) = sample();
    let doc = split(&p, &drive_ref).unwrap();
    assert_eq!(doc.sequences.keys().copied().collect::<Vec<_>>(), vec![s1, s2]);
    let q = merge(&doc, &resolve_back).unwrap();
    assert_eq!(media_path(&q, a), "/Volumes/Drive/Team/Media/A001.mov");
    assert_eq!(media_path(&q, b), "/Volumes/Drive/Team/Media/A002.mov");
    // the local-only item keeps only its file name; this Mac's resolver does not know it
    assert_eq!(media_path(&q, c), "phone.mov");
    // equal except paths, proxies and local settings
    let mut expected = p.clone();
    for (id, path) in [(c, "phone.mov")] {
        if let ItemKind::Media(m) = &mut expected.items.get_mut(&id).unwrap().kind {
            m.media = MediaRef::File { path: path.into() };
        }
    }
    for m in expected.items.values_mut() {
        if let ItemKind::Media(m) = &mut m.kind {
            m.proxy = None;
        }
    }
    expected.settings.scratch.auto_save = None;
    assert_eq!(as_json(&q), as_json(&expected));
}

#[test]
fn split_strips_proxy_and_local_paths() {
    let (p, _, _) = sample();
    let doc = split(&p, &drive_ref).unwrap();
    let all = serde_json::to_string(&(&doc.shared, &doc.sequences)).unwrap();
    assert!(!all.contains("/Users/"), "{all}");
    assert!(!all.contains("/Volumes/"), "{all}");
    assert!(!all.contains("proxy.mov"), "{all}");
    assert!(all.contains("drive:/Team/Media/A001.mov"));
    assert!(all.contains("\"local_only\":true"));
}

#[test]
fn merge_fills_proxy_from_cache() {
    let (p, [a, ..], _) = sample();
    let doc = split(&p, &drive_ref).unwrap();
    let q = merge(&doc, &|_| LocalMedia { original: None, proxy: Some(PathBuf::from("/cache/proxy.mov")) }).unwrap();
    let m = q.item(a).unwrap().as_media().unwrap();
    assert_eq!(m.media, MediaRef::File { path: "drive:/Team/Media/A001.mov".into() });
    assert_eq!(m.proxy, Some(MediaRef::File { path: "/cache/proxy.mov".into() }));
}

#[test]
fn generators_are_kept() {
    let mut p = Project::new("Gen");
    let info = filmcraft_media::generators::GeneratorSource::demo(DemoScene::OceanSunset).info().clone();
    let g = p.add_item(
        "Ocean",
        Label::Iris,
        ItemKind::Media(Box::new(MediaClip {
            media: MediaRef::Generator(Generator::Demo(DemoScene::OceanSunset)),
            info,
            interpret: Default::default(),
            mark_in: None,
            mark_out: None,
            markers: vec![],
            offline: false,
            proxy: None,
            identity: None,
        })),
        None,
    );
    let q = merge(&split(&p, &drive_ref).unwrap(), &resolve_back).unwrap();
    assert_eq!(q.item(g), p.item(g));
}

#[test]
fn replace_sequence_only_touches_that_item() {
    let (mut p, _, [s1, s2]) = sample();
    let mut other = p.clone();
    other.items.get_mut(&s1).unwrap().name = "Package v2".into();
    other.sequence_mut(s1).unwrap().video_tracks[0].items.clear();
    let json = sequence_json(&other, s1).unwrap();
    let before = p.clone();
    replace_sequence(&mut p, s1, &json).unwrap();
    assert_eq!(as_json(&p.item(s1)), as_json(&other.item(s1)));
    assert_eq!(as_json(&p.item(s2)), as_json(&before.item(s2)));
    let mut rest_after = p.clone();
    let mut rest_before = before;
    rest_after.items.remove(&s1);
    rest_before.items.remove(&s1);
    assert_eq!(as_json(&rest_after), as_json(&rest_before));
}

#[test]
fn replace_sequence_refuses_bad_input() {
    let (mut p, [a, ..], [s1, s2]) = sample();
    let before = p.clone();
    // the JSON's id must match
    let json = sequence_json(&p, s2).unwrap();
    assert!(replace_sequence(&mut p, s1, &json).is_err());
    // a media item is not a sequence
    let media = serde_json::to_value(p.item(a).unwrap()).unwrap();
    assert!(replace_sequence(&mut p, a, &media).is_err());
    // nor is garbage
    assert!(replace_sequence(&mut p, s1, &Value::String("x".into())).is_err());
    assert_eq!(as_json(&p), as_json(&before));
    assert!(sequence_json(&p, a).is_none());
}

#[test]
fn merge_refuses_corrupt_parts() {
    let (p, _, [s1, _]) = sample();
    let mut doc = split(&p, &drive_ref).unwrap();
    doc.shared = Value::Null;
    assert!(merge(&doc, &resolve_back).is_err());
    let mut doc = split(&p, &drive_ref).unwrap();
    let wrong = doc.sequences.values().nth(1).cloned().unwrap();
    doc.sequences.insert(s1, wrong);
    assert!(merge(&doc, &resolve_back).is_err());
    let _: BTreeMap<ItemId, Value> = doc.sequences;
}

/// Two members import at the same moment from the same base. The shared part merges items by id
/// (as the Drive does), and both imports survive with different ids.
#[test]
fn test_shared_add_merges_concurrent() {
    let (base, _, _) = sample();
    let base_doc = split(&base, &drive_ref).unwrap();
    let (mut a, mut b) = (base.clone(), base.clone());
    a.set_id_slot(1).unwrap();
    b.set_id_slot(2).unwrap();
    let from_a = a.add_item("B-roll.mov", Label::Iris, media("/Volumes/Drive/Team/Media/B-roll.mov"), None);
    let from_b = b.add_item("Interview.mov", Label::Iris, media("/Volumes/Drive/Team/Media/Interview.mov"), None);
    assert_ne!(from_a, from_b);
    let mut shared = base_doc.shared.clone();
    for member in [&a, &b] {
        let doc = split(member, &drive_ref).unwrap();
        let items = doc.shared["items"].as_object().unwrap().clone();
        let merged = shared["items"].as_object_mut().unwrap();
        for (id, item) in items {
            merged.insert(id, item);
        }
    }
    let q = merge(&TeamDoc { shared, sequences: base_doc.sequences.clone() }, &resolve_back).unwrap();
    assert_eq!(media_path(&q, from_a), "/Volumes/Drive/Team/Media/B-roll.mov");
    assert_eq!(media_path(&q, from_b), "/Volumes/Drive/Team/Media/Interview.mov");
    assert_eq!(q.items.len(), base.items.len() + 2);
}
