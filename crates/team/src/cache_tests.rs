//! The media cache rules (`cache.rs`): which media is wanted, and what is removed when.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use filmcraft_media::{DemoScene, MediaSource};
use filmcraft_project::{ItemId, ItemKind, Label, MediaClip, MediaIdentity, MediaRef, Project, SequenceSettings, TrackKind};
use filmcraft_time::{TICKS_PER_SECOND, Tick, TimeRange};

use crate::cache::*;

const DAY: u64 = 24 * 60 * 60;
const GB: u64 = 1_000_000_000;

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
        proxy: None,
        identity: None,
    }))
}

fn put(p: &mut Project, seq: ItemId, item: ItemId) {
    let rate = p.sequence(seq).unwrap().settings.frame_rate;
    let ti = p.make_track_item(item, TrackKind::Video, Tick::ZERO, TimeRange::new(Tick::ZERO, Tick(2 * TICKS_PER_SECOND)), rate).unwrap();
    p.sequence_mut(seq).unwrap().video_tracks[0].items.push(ti);
}

#[test]
fn wanted_includes_nested_sequence_media() {
    let mut p = Project::new("Robotics");
    let a = p.add_item("A001.mov", Label::Iris, media("drive:/Team Projects/R Media/A001.mov"), None);
    let b = p.add_item("A002.mov", Label::Iris, media("drive:/Team Projects/R Media/A002.mov"), None);
    let c = p.add_item("A003.mov", Label::Iris, media("drive:/Team Projects/R Media/A003.mov"), None);
    let d = p.add_item("A004.mov", Label::Iris, media("drive:/Team Projects/R Media/A004.mov"), None);
    let sub = p.add_item(
        "A002 sub",
        Label::Iris,
        ItemKind::Subclip { parent: b, range: TimeRange::new(Tick::ZERO, Tick(TICKS_PER_SECOND)), restrict_trims: false },
        None,
    );
    let main = p.new_sequence("Package", SequenceSettings::default(), 2, 2, None);
    let nest = p.new_sequence("Interview", SequenceSettings::default(), 2, 2, None);
    let deeper = p.new_sequence("B-roll", SequenceSettings::default(), 2, 2, None);
    let other = p.new_sequence("Teaser", SequenceSettings::default(), 2, 2, None);
    put(&mut p, main, a);
    put(&mut p, main, nest);
    put(&mut p, nest, sub);
    put(&mut p, nest, deeper);
    put(&mut p, deeper, nest); // a cycle must not loop forever
    put(&mut p, other, c);
    assert_eq!(wanted(&p, &[main], None), BTreeSet::from([a, b]));
    assert_eq!(wanted(&p, &[main], Some(d)), BTreeSet::from([a, b, d]));
    assert_eq!(wanted(&p, &[other], None), BTreeSet::from([c]));
    assert_eq!(wanted(&p, &[], None), BTreeSet::new(), "opening a project downloads nothing");
    assert_eq!(wanted(&p, &[ItemId(999_999)], None), BTreeSet::new());
}

fn fp(n: u64) -> Fingerprint {
    Fingerprint::of(&MediaIdentity { size: n, fingerprint: n })
}

fn file(name: &str, bytes: u64) -> CachedFile {
    CachedFile { path: PathBuf::from(format!("/cache/{name}")), bytes }
}

/// Three media files of project "p" (opened at `now`): fp(1) used longest ago, fp(3) last.
fn three(now: u64) -> CacheIndex {
    let mut idx = CacheIndex::default();
    idx.opened("p", now);
    for n in 1..=3 {
        let r = format!("drive:/Team Projects/P Media/{n}.mov");
        idx.record_original(&fp(n), "p", &r, file(&format!("{n}/original"), 50 * GB), now - 10 * DAY + n);
        idx.record_proxy(&fp(n), "p", &r, file(&format!("{n}/proxy"), 5 * GB), now - 10 * DAY + n);
    }
    idx
}

fn paths(v: &[&str]) -> Vec<PathBuf> {
    v.iter().map(|s| PathBuf::from(format!("/cache/{s}"))).collect()
}

#[test]
fn eviction_originals_before_proxies() {
    let now = 100 * DAY;
    let idx = three(now);
    assert_eq!(idx.total_bytes(), 165 * GB);
    assert!(eviction_plan(&idx, 200 * GB, now, &BTreeSet::new()).is_empty(), "under the cap nothing goes");
    // 165 → 100: the two least recently used originals go
    assert_eq!(eviction_plan(&idx, 100 * GB, now, &BTreeSet::new()), paths(&["1/original", "2/original"]));
    // 165 → 10: every original, then proxies oldest first
    assert_eq!(eviction_plan(&idx, 10 * GB, now, &BTreeSet::new()), paths(&["1/original", "2/original", "3/original", "1/proxy"]));
}

#[test]
fn eviction_skips_media_of_open_sequences() {
    let now = 100 * DAY;
    let idx = three(now);
    let in_use = BTreeSet::from([fp(1)]);
    assert_eq!(eviction_plan(&idx, 100 * GB, now, &in_use), paths(&["2/original", "3/original"]));
    let all = BTreeSet::from([fp(1), fp(2), fp(3)]);
    assert!(eviction_plan(&idx, 0, now, &all).is_empty(), "media of an open sequence is never removed, even over the cap");
}

#[test]
fn thirty_day_project_evicted() {
    let now = 100 * DAY;
    let mut idx = three(now);
    // fp(1) is shared with a project opened 5 days later; fp(2) and fp(3) belong only to "p"
    idx.record_original(&fp(1), "q", "drive:/x", file("1/original", 50 * GB), now - 40 * DAY);
    idx.opened("q", now + 5 * DAY);
    assert!(eviction_plan(&idx, 200 * GB, now + 29 * DAY, &BTreeSet::new()).is_empty(), "29 days: kept");
    let plan = eviction_plan(&idx, 200 * GB, now + 30 * DAY, &BTreeSet::new());
    assert_eq!(plan, paths(&["2/original", "2/proxy", "3/original", "3/proxy"]));
    // open sequences keep their media even in an idle project
    let plan = eviction_plan(&idx, 200 * GB, now + 30 * DAY, &BTreeSet::from([fp(2)]));
    assert_eq!(plan, paths(&["3/original", "3/proxy"]));
    // the other project goes idle too: everything
    assert_eq!(eviction_plan(&idx, 200 * GB, now + 35 * DAY, &BTreeSet::new()).len(), 6);
}

#[test]
fn index_round_trips_and_forgets_removed_files() {
    let dir = std::env::temp_dir().join(format!("filmcraft-team-cache-{}-{:?}", std::process::id(), std::thread::current().id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut idx = three(50 * DAY);
    idx.save(&dir).unwrap();
    assert_eq!(CacheIndex::load(&dir), idx);
    let r = "drive:/Team Projects/P Media/2.mov";
    assert_eq!(idx.resolve(r).original, Some(PathBuf::from("/cache/2/original")));
    assert_eq!(idx.drive_ref_of(Path::new("/cache/2/proxy")), Some(r));
    idx.forget(&BTreeSet::from([PathBuf::from("/cache/2/original"), PathBuf::from("/cache/3/original"), PathBuf::from("/cache/3/proxy")]));
    assert_eq!(idx.resolve(r).original, None);
    assert_eq!(idx.resolve(r).proxy, Some(PathBuf::from("/cache/2/proxy")));
    assert!(!idx.entries.contains_key(&fp(3)), "an entry with no files left goes");
    // a damaged index, or one with a hostile key, loads as empty instead of failing
    std::fs::write(dir.join(INDEX_FILE), br#"{"entries": {"../../etc": {}}}"#).unwrap();
    assert_eq!(CacheIndex::load(&dir), CacheIndex::default());
    std::fs::write(dir.join(INDEX_FILE), b"{not json").unwrap();
    assert_eq!(CacheIndex::load(&dir), CacheIndex::default());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fingerprints_are_safe_folder_names() {
    assert_eq!(fp(255).as_str(), "00000000000000ff-255");
    let by_path = Fingerprint::of_drive_path("drive:/Team Projects/P Media/../x.mov");
    assert!(by_path.as_str().starts_with("p-") && by_path.as_str().len() == 18);
    assert_eq!(Fingerprint::for_media(None, "drive:/a"), Fingerprint::of_drive_path("drive:/a"));
    for bad in ["", "..", "a/b", "-x", "A", &"a".repeat(65)] {
        assert!(Fingerprint::try_from(bad.to_string()).is_err(), "{bad:?}");
    }
}

#[test]
fn drive_ref_only_for_media_not_yet_here() {
    let mut p = Project::new("R");
    let a = p.add_item("A", Label::Iris, media("drive:/Team Projects/R Media/A.mov"), None);
    let b = p.add_item("B", Label::Iris, media("/Users/someone/Caches/B.mov"), None);
    assert_eq!(drive_ref(&p, a), Some("drive:/Team Projects/R Media/A.mov"));
    assert_eq!(drive_ref(&p, b), None);
    assert_eq!(drive_ref(&p, ItemId(77)), None);
}
