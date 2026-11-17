//! `filmcraft-cli team bench <project-id> <file> [--runs N]`: how fast media moves between this
//! machine and the Drive, the old way (one stream per file) against the current one (parallel
//! ranges and upload chunks). Uploads `<file>` into the project's media folder under `Bench/` (a
//! new name each run), downloads it back (checked against the original's SHA-256) and prints MB/s as JSON. For a dev or
//! test Drive: it leaves the uploaded copies in the project's media folder.

use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use filmcraft_drive::{DriveClient, DriveConfig, Expected, Transfers};
use serde_json::{Value, json};

use crate::args::Args;
use crate::team::{app_info, dir_of, read};

/// Largest number of runs per mode.
const MAX_RUNS: u32 = 10;

pub fn bench(a: &Args) -> Result<(), String> {
    let dir = dir_of(a)?;
    let (project, file) = match (a.pos(2), a.pos(3)) {
        (Some(p), Some(f)) => (p.to_string(), f.to_string()),
        _ => return Err("usage: team bench <project-id> <file> [--runs N]".into()),
    };
    let runs: u32 = a.opt("--runs").map_or(Ok(1), str::parse).map_err(|_| "--runs takes a number")?;
    let runs = runs.clamp(1, MAX_RUNS);
    let cfg = DriveConfig::new(&read(&dir, "drive_url")?, None).map_err(|e| e.to_string())?;
    let token = read(&dir, "token")?;
    let device = read(&dir, "device").unwrap_or_else(|_| "filmcraft-cli".into());
    let src = Path::new(&file);
    let size = std::fs::metadata(src).map_err(|e| format!("{file}: {e}"))?.len();
    let mut f = std::fs::File::open(src).map_err(|e| format!("{file}: {e}"))?;
    let sha = filmcraft_drive::sha256_hex(&mut f).map_err(|e| format!("{file}: {e}"))?;
    let work = dir.join("bench");
    std::fs::create_dir_all(&work).map_err(|e| format!("{}: {e}", work.display()))?;
    let mut results = Vec::new();
    for (mode, transfers) in [("one stream", Transfers::serial()), ("parallel", Transfers::default())] {
        let client = DriveClient::connect(&cfg, &token, app_info(&device)?).map_err(|e| e.to_string())?.with_transfers(transfers);
        for run in 0..runs {
            results.push(one_run(&client, &project, src, size, &sha, &work, mode, run)?);
        }
    }
    let _ = std::fs::remove_dir_all(&work);
    outln!("{}", json!({"ok": true, "bytes": size, "runs": results}));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn one_run(client: &DriveClient, project: &str, src: &Path, size: u64, sha: &str, work: &Path, mode: &str, run: u32) -> Result<Value, String> {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
    let ext = src.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let name = format!("bench-{}-{stamp}-{run}{ext}", mode.replace(' ', "-"));
    let started = Instant::now();
    let up = client.upload_media(project, src, &name, Some("Bench"), None, &mut |_| true).map_err(|e| format!("upload: {e}"))?;
    let up_s = started.elapsed().as_secs_f64();
    let dest = work.join(&name);
    let started = Instant::now();
    let rel = format!("Bench/{}", up.name);
    client
        .download_media(project, &rel, &dest, &Expected { size: Some(size), sha256: Some(sha.to_string()) }, &mut |_, _| true)
        .map_err(|e| format!("download: {e}"))?;
    let down_s = started.elapsed().as_secs_f64();
    // the download checked its SHA-256 against the original's
    let _ = std::fs::remove_file(&dest);
    let mbps = |secs: f64| if secs > 0.0 { (size as f64 / 1e6 / secs * 10.0).round() / 10.0 } else { 0.0 };
    Ok(json!({
        "mode": mode,
        "on_lan": client.on_lan(),
        "upload_s": (up_s * 100.0).round() / 100.0,
        "upload_mb_s": mbps(up_s),
        "download_s": (down_s * 100.0).round() / 100.0,
        "download_mb_s": mbps(down_s),
    }))
}
