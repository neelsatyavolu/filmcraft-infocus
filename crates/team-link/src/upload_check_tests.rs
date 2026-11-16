//! Uploads: files are checked off the UI thread, and a file changed since its upload started goes
//! up again from the start, against the in-process Drive of `upload_tests.rs`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use filmcraft_project::{Label, Project};
use filmcraft_team::cache::CacheIndex;

use crate::tests::{PID, client};
use crate::upload_tests::{Drive, MIB, drive, footage, media_item, poll_finished, queue_all};
use crate::*;

#[test]
fn scan_leaves_checking_new_files_to_a_worker() {
    let state = Arc::new(Mutex::new(Drive::default()));
    let server = drive(state);
    let (disk, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, _) = footage(disk.path(), "a.mov", 100);
    let mut p = Project::new("Robotics");
    let ia = p.add_item("a", Label::Rose, media_item(&a.to_string_lossy()), None);
    let mut up = TeamUploads::new(cache.path().to_path_buf(), PID, client(&server.url, "ifd_test"), Arc::default());
    // the UI thread only lists what to look at (a file on a sleeping disk would block it)
    assert!(up.scan(&p, &CacheIndex::default()).is_empty());
    assert!(up.checking());
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut queued = Vec::new();
    while queued.is_empty() {
        assert!(Instant::now() < deadline);
        queued = up.take_queued();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(queued, vec![ia]);
    assert_eq!(up.pending().len(), 1);
}

#[test]
fn a_file_changed_since_its_upload_started_goes_up_again() {
    let state = Arc::new(Mutex::new(Drive { slow_chunks: true, ..Default::default() }));
    let server = drive(state.clone());
    let (disk, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, _) = footage(disk.path(), "a.mov", 3 * MIB as usize);
    let mut p = Project::new("Robotics");
    p.add_item("a", Label::Rose, media_item(&a.to_string_lossy()), None);
    let mut up = TeamUploads::new(cache.path().to_path_buf(), PID, client(&server.url, "ifd_test"), Arc::default());
    queue_all(&mut up, &p, &CacheIndex::default());
    let deadline = Instant::now() + Duration::from_secs(10);
    while !up.progress().iter().any(|(_, f)| *f > 0.0) {
        assert!(Instant::now() < deadline, "no progress");
        up.poll();
        std::thread::sleep(Duration::from_millis(10));
    }
    up.pause(true);
    while up.progress().len() == 1 {
        assert!(Instant::now() < deadline);
        up.poll();
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(up);
    // edited in another app before the next run: same size, new bytes, new modification time
    let changed: Vec<u8> = (0..3 * MIB as usize).map(|i| (i % 13) as u8).collect();
    std::fs::write(&a, &changed).unwrap();
    std::fs::File::options().write(true).open(&a).unwrap().set_modified(SystemTime::now() + Duration::from_secs(60)).unwrap();

    let mut up = TeamUploads::new(cache.path().to_path_buf(), PID, client(&server.url, "ifd_test"), Arc::default());
    queue_all(&mut up, &p, &CacheIndex::default());
    poll_finished(&mut up, 1);
    assert_eq!(state.lock().unwrap().files["a.mov"], changed, "the Drive has the file as it is now, not half old chunks");
    assert_eq!(server.requests().iter().filter(|r| r.path().ends_with("/media/upload/init")).count(), 2, "a new session");
}
