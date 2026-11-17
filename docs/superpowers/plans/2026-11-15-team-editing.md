# Team Editing (InFocus Cut) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let 2 to 3 InFocus members edit one FilmCraft project from their own Macs, with one editor per sequence, sync through InFocus Drive, local proxies for speed, and automatic cleanup on Mac and Drive.

**Architecture:** Two repos. `infocus-drive` gets a team-project service (FastAPI router plus SQLite state) that owns locks, sequence revisions, a change feed and daily cleanup. `filmcraft-infocus` (public fork) gets two new crates: `filmcraft-team` (pure logic: id slots, project split and merge, change classification, media cache rules) and `filmcraft-drive` (HTTP client and sign-in). It also gets thin hooks in `filmcraft-engine` (after-command check, remote apply) and `filmcraft-ui-egui` (team panel, lock badges). Sync detects changes by comparing the project before and after each command, not with a per-command table.

**Tech Stack:** Rust 1.95 workspace (egui), `ureq` (blocking HTTP), `keyring` (macOS Keychain); Python 3 FastAPI, SQLite, pytest.

**Spec:** `/Users/neel/Documents/GitHub/infocus-drive/docs/superpowers/specs/2026-11-15-filmcraft-team-editing-design.md` (copied to `docs/team-editing-design.md` in the fork in Task F0).

## Global Constraints

- Lock: heartbeat every **20 s**, expiry **120 s** after the last heartbeat; admins can force-release.
- Push debounce **1 s** after each completed undo step; viewers see updates via SSE, with polling every **5 s** as the fallback.
- Ids: `slot << 40 | counter`; slot 0 = imported single-user ids; slots 1..=1023 handed out per member per project. All ids stay < 2^53, which keeps them safe as JSON numbers.
- Media cache: `~/Library/Caches/InFocus Cut/media/<fingerprint>/{original,proxy}`; size cap **200 GB**; evict originals first, oldest first; never evict a proxy in use. Per-project local eviction after **30 days** not opened on this Mac.
- Downloads: only for sequences the member opens and clips loaded to Source or dropped on a timeline; at most **2** concurrent downloads; paused during export; resume with HTTP Range.
- Proxies: FilmCraft's existing `proxies.rs` job, ProRes 422 Proxy at ½ size; proxies on by default in team projects.
- Drive cleanup: notice at **23 days** without an open (7 days before), move to `#recycle` at **30 days**; skip "Keep forever" projects and projects with a held lock or an active export; move only `<name>.fcteam/` and the project's own `Media/`; log every move.
- Team projects live under Drive root `Team Projects/`; project id in `meta.json`; the server rejects writes from apps below the project's `min_app_version` or with an older `schema_version`.
- `MediaClip::proxy` and local file paths never leave the Mac; a team media ref is `drive:/<path>` plus the fingerprint.
- infocus-drive is a **public repo with minors as users**: fictional values only in tests and docs, no hosts, IPs or names; run `gitleaks git .` before each commit; file IO only through `fsops` under `as_user`; never hold the `as_user` lock across lock, SSE or SQLite work.
- Fork: InFocus hosts come from config (`~/Library/Application Support/InFocus Cut/config.toml` or build env `INFOCUS_DRIVE_URL`), never hard-coded.
- Builds on the owner's laptop: `nice -n 15`, `CARGO_BUILD_JOBS=6`.
- Fork follows `AGENTS.md`: no `unwrap/expect/panic` or `unsafe` outside tests (new crate roots copy the `deny` attribute and `[lints] workspace = true`); layering via `cargo xtask layers` (`filmcraft-team` and `filmcraft-drive` must not depend on egui; the engine must not depend on `filmcraft-drive`); dependency licences on the allowlist; gate `cargo xtask ci` (or at least fmt, clippy -D warnings and the touched crates' tests) before each commit; worker threads under `catch_unwind`.
- Forks must remove the ArtCraft brand marks (`docs/brand/`, the README header logos) under `docs/brand/LICENSE-brand.txt`; Task F0 does this.
- Fork work goes on branch `feat/team-editing`, not `main`; infocus-drive work on `feat/team-projects`. PRs only, no merges, no releases.

## Review Focus

1. Mac sleeps mid-edit, lock expires, a teammate takes it, then the first Mac wakes and pushes: the push must be rejected, the edits saved as "Recovered – <name>", and nothing lost (Task F5 test `lost_lock_saves_recovered_copy`).
2. Two members import media at the same moment: no id collision, and both items survive (Task F1 `slots_never_collide`, Task D2 `test_shared_add_merges_concurrent`).
3. A download interrupted halfway (laptop closed, Wi-Fi drops): it resumes from the byte it stopped at and never attaches a partial file as an original (Task F6 `resume_from_partial`, `partial_never_promoted`).
4. A single-user `.fcproj` with existing ids is turned into a team project: ids keep working and new ids don't collide with old ones (Task F1 `slot_zero_keeps_legacy_ids`).
5. An edit that touches a sequence someone else has locked (for example ripple delete in a nested sequence, or a paste into a read-only tab): it is reverted with a clear message, and the project is unchanged (Task F5 `edit_touching_foreign_lock_reverts`).

---

## Part D: infocus-drive (branch `feat/team-projects`)

### Task D1: Team store (state and lock rules)

**Files:**
- Create: `app/team_store.py`
- Test: `tests/test_team_store.py`

**Interfaces:**
- Produces: `TeamStore(db_path: Path, clock: Callable[[], float] = time.time)` with:
  - `create_project(folder_rel: str, name: str, creator: str) -> str` (project id, `uuid4().hex`)
  - `slot_for(project_id: str, user: str) -> int` (1..=1023, stable per user)
  - `take_lock(project_id, seq_id: int, user) -> LockResult` (`LockResult(ok: bool, token: str|None, holder: str|None, expires_at: float)`)
  - `heartbeat(project_id, seq_id, token) -> bool`, `release(project_id, seq_id, token) -> bool`, `force_release(project_id, seq_id) -> None`
  - `locks(project_id) -> dict[int, tuple[str, float]]`
  - `bump_revision(project_id, part: str, base: int) -> int | None` (`part` is `"shared"` or `f"seq:{id}"`; None if `base` is stale)
  - `revision(project_id, part) -> int`
  - `record_open(project_id, user) -> None`, `last_open(project_id) -> float`, `members(project_id) -> list[str]`
  - `set_keep(project_id, keep: bool) -> None`, `is_keep(project_id) -> bool`
  - `stale_projects(older_than_s: float) -> list[str]`; `mark_notified(project_id)`, `notified(project_id) -> bool` (cleared by `record_open`)
- Constants: `HEARTBEAT_S = 20`, `LOCK_TTL_S = 120`.

- [ ] **Step 1: Write failing tests:** `test_take_lock_free`, `test_second_taker_refused_with_holder`, `test_lock_expires_after_ttl` (clock +121 → another user gets it), `test_heartbeat_extends`, `test_release_requires_token`, `test_concurrent_take_exactly_one_wins` (20 threads, `sum(ok) == 1`), `test_stale_base_revision_rejected`, `test_slot_stable_and_unique`, `test_slots_capped_at_1023`, `test_record_open_clears_notified`, `test_stale_projects_skips_keep`.
- [ ] **Step 2:** `cd /Users/neel/Documents/GitHub/infocus-drive && python -m pytest tests/test_team_store.py -q`, expect failures (module missing).
- [ ] **Step 3:** Implement with `sqlite3` and `BEGIN IMMEDIATE` for take and bump (that's what makes the concurrency test pass). DB path from a new setting `TEAM_DB_PATH` (default `/config/team_projects.sqlite3`; conftest sets a tmp path like `CLI_TOKENS_DB_PATH`).
- [ ] **Step 4:** Tests pass.
- [ ] **Step 5:** Commit `feat(team): lock and revision store for team projects`.

### Task D2: Team projects API

**Files:**
- Create: `app/team_projects.py` (router), `app/team_files.py` (reads and writes `.fcteam` files via `fsops` as the user)
- Modify: `app/main.py` (one `include_router` line next to line 140), `app/config.py` (`TEAM_DB_PATH`, `TEAM_ROOT_REL = "Team Projects"`), `tests/conftest.py` (tmp `TEAM_DB_PATH`)
- Test: `tests/test_team_projects.py`

**Interfaces:**
- Consumes: Task D1 `TeamStore`; existing `_bearer_token`/`cli_tokens.lookup` and the session user (follow how `/api/cli/sessions` resolves the user in `main.py`); `fsops.as_user`, `fsops.resolve_rel`.
- Produces (all JSON, auth = bearer `ifd_` token or session cookie):
  - `GET /api/team/projects` → `[{id, name, folder, locks, last_open}]` (folders under `Team Projects/` the user can read)
  - `POST /api/team/projects` body `{name, project?: <fcproj project JSON>, app_version, schema_version}` → `{id, slot}`
  - `GET /api/team/projects/{id}` → `{id, name, meta, shared, sequences: {seq_id: {rev, json}}, shared_rev, locks, slot}`; also records an open
  - `POST …/sequences/{seq}/lock` → `{token, expires_at}` or 409 `{holder}`; `POST …/lock/heartbeat` `{token}`; `DELETE …/lock` `{token}`; `POST …/lock/force` (admin only)
  - `PUT …/sequences/{seq}` `{token, base_rev, json, app_version, schema_version}` → `{rev}`; 409 stale or lock lost; 426 old app
  - `POST …/shared` `{base_rev, upserts: {item_id: item_json}, deletes: [item_id], bins?: <root bin JSON>}` → `{rev}`; upserts merge by id; a delete of an item referenced by any sequence returns 409
  - `GET …/events` SSE (`sequence_updated {seq, rev, by}`, `lock_changed {seq, holder}`, `shared_updated {rev}`), with a keepalive comment every 15 s
  - `POST …/keep` `{keep: bool}` (creator or admin); `POST …/open` (reset the cleanup clock)
- On-disk layout (written by `team_files.py` only): `Team Projects/<name>.fcteam/{project.json, sequences/<id>.json, meta.json}` and `Team Projects/<name>.fcteam/../<name> Media/`. *(The design's `Media/` sits beside the `.fcteam` as `<name> Media/` so the web UI can show it as a normal folder.)*
- `meta.json`: `{id, name, schema_version, min_app_version, created_by, file_hashes: {relpath: sha256}}`. On read, if a file's hash isn't the recorded one, respond 409 `{"error": "project files were changed outside InFocus Cut"}`.

- [ ] **Step 1: Write failing tests** with FastAPI `TestClient`, a tmp drive root and a fake `as_user` (follow `tests/test_cli_api.py` for auth fixtures): `test_create_and_get`, `test_lock_conflict_returns_holder`, `test_put_requires_current_lock`, `test_put_stale_base_rejected`, `test_shared_add_merges_concurrent` (two upserts from base 0 → both kept), `test_delete_referenced_item_refused`, `test_old_app_rejected_426`, `test_hash_mismatch_409`, `test_user_without_folder_access_403`, `test_events_stream_sends_sequence_updated`, `test_force_release_admin_only`, `test_as_user_lock_not_held_during_sse` (assert `fsops._as_user_lock` is free while a stream is open).
- [ ] **Step 2:** Run; expect failures.
- [ ] **Step 3:** Implement. SSE: an in-process `asyncio.Queue` per subscriber keyed by project id, published after each successful write (single server process, same as the token reuse cache).
- [ ] **Step 4:** Tests pass, plus the full suite `python -m pytest -q`.
- [ ] **Step 5:** `gitleaks git .`, then commit `feat(team): team project API with locks and change feed`.

### Task D3: Daily cleanup

**Files:**
- Create: `app/team_cleanup.py`
- Modify: `app/main.py` (start the daily task on startup, same pattern as other background loops in main; find with `grep -n "on_event\|lifespan" app/main.py`)
- Test: `tests/test_team_cleanup.py`

**Interfaces:**
- Consumes: `TeamStore.stale_projects/notified/mark_notified/is_keep/locks/members`, `team_files`, the existing recycle helper in `fsops` (`grep -n recycle app/fsops.py`), the existing Resend sender (`grep -n resend app/email_auth.py`).
- Produces: `run_cleanup(store, now: float, send_email, move_to_recycle, log) -> CleanupReport(notified: list[str], moved: list[str], skipped: dict[str, str])`; constants `NOTICE_AFTER_S = 23*86400`, `MOVE_AFTER_S = 30*86400`. `POST /api/team/exports/{id}/begin|end` mark an export in progress (in-memory, expires after 6 h).

- [ ] **Step 1: Write failing tests:** `test_notice_sent_once_at_23_days`, `test_moved_at_30_days_to_recycle`, `test_keep_forever_never_moved`, `test_locked_project_skipped`, `test_export_in_progress_skipped`, `test_only_project_folders_moved` (a sibling folder survives), `test_open_resets_clock`, `test_move_logged`.
- [ ] **Step 2:** Run; expect failures.
- [ ] **Step 3:** Implement as a pure function with injected effects; the loop runs it once a day at 03:00 server time. Email copy: subject "Your InFocus Cut project “<name>” will be cleaned up in 7 days", body with the date, and a link to `/team/keep?project=<id>` (a page with one Keep button that calls `POST …/open`).
- [ ] **Step 4:** Tests pass.
- [ ] **Step 5:** Commit `feat(team): daily cleanup of unopened team projects`.

### Task D4: Web UI treats `.fcteam` as one item

**Files:**
- Modify: `app/static/app.js` (listing render), `app/main.py` (refuse mutations inside `*.fcteam/` from web routes: rename, delete, upload, mkdir, move; deleting the whole `.fcteam` is still allowed)
- Test: `tests/test_team_projects.py::test_web_mutation_inside_fcteam_refused`, and a `.cjs` test following `tests/test_drive_links.cjs` for rendering (`.fcteam` shows as one item with the "InFocus Cut project" kind and no open-folder click)

- [ ] **Step 1:** Write failing tests. **Step 2:** Run. **Step 3:** Implement; bump the `?v=` cache-bust in `index.html` and `app.js` imports. **Step 4:** Tests pass. **Step 5:** Commit `feat(team): show team projects as one item in the web UI`.

---

## Part F: filmcraft-infocus (fork, branch `main`)

### Task F0: Fork setup

**Files:** Create `docs/team-editing.md` (what the fork adds, the list of engine and UI hooks, the upstream merge routine), `docs/team-editing-design.md` (copy of the spec), `config/infocus.example.toml` (`drive_url = "https://drive.example.com"`, `lan_url = "http://192.168.1.50:8790"`). Modify `README.md`: one short "InFocus Cut fork" note at the top.
- [ ] Verify `nice -n 15 env CARGO_BUILD_JOBS=6 cargo build -p filmcraft --release` passes on the clean fork (record the build time in `docs/team-editing.md`). Commit `docs: InFocus Cut fork notes and design`.

### Task F1: Id slots

**Files:**
- Modify: `crates/project/src/lib.rs` (`Project`: add `#[serde(skip)] pub id_slot: u16`; `alloc_id`)
- Test: `crates/project/src/tests.rs`

**Interfaces:**
- Produces: `Project::set_id_slot(&mut self, slot: u16)` (sets `id_slot`, and sets `next_id` to `max(next_id, slot<<40 | 1)` if the current `next_id` is below the slot base, else keeps it); `alloc_id` returns `next_id` and increments, as before. `pub const ID_SLOT_SHIFT: u32 = 40`.
- Slot 0 behaves exactly like upstream.

- [ ] **Step 1: Failing tests:** `slot_zero_keeps_legacy_ids` (a project with next_id 48 at slot 0 allocates 48), `slot_ids_start_at_base` (slot 3 → first id `3<<40 | 1`), `slots_never_collide` (two clones of one project at slots 1 and 2 each alloc 1000 ids; the sets are disjoint), `slot_survives_reload_via_set` (serialize → deserialize → `set_id_slot` again → continues past the max existing id in that slot, using `max_id` restricted to the slot range).
- [ ] **Step 2:** `cargo test -p filmcraft-project slot` fails. **Step 3:** Implement. **Step 4:** Passes, plus `cargo test -p filmcraft-project`. **Step 5:** Commit `feat(project): per-member id slots for team projects`.

### Task F2: `filmcraft-team` crate: split, merge, strip

**Files:**
- Create: `crates/team/Cargo.toml` (deps: `filmcraft-project`, `serde`, `serde_json`), `crates/team/src/lib.rs`, `crates/team/src/doc.rs`, `crates/team/src/tests.rs`

**Interfaces:**
- Produces:
  - `pub struct TeamDoc { pub shared: serde_json::Value, pub sequences: BTreeMap<ItemId, serde_json::Value> }`
  - `pub fn split(p: &Project, media_ref: &dyn Fn(&MediaRef) -> Option<String>) -> TeamDoc`: each sequence item goes to `sequences`; everything else goes to `shared`; `MediaClip::proxy` is cleared; each media path becomes `drive:/…` via `media_ref`. Media outside the Drive returns `None` → `split` keeps the path but marks it `"local_only": true`.
  - `pub fn merge(doc: &TeamDoc, resolve: &dyn Fn(&str) -> LocalMedia) -> Result<Project, TeamError>`, where `LocalMedia { original: Option<PathBuf>, proxy: Option<PathBuf> }` fills in paths and proxies from the local cache.
  - `pub fn sequence_json(p: &Project, id: ItemId) -> Option<Value>`; `pub fn replace_sequence(p: &mut Project, id: ItemId, json: &Value) -> Result<(), TeamError>`

- [ ] **Step 1: Failing tests:** `split_merge_round_trip` (the demo project from `filmcraft-engine` demo or a hand-built one with 2 sequences and 3 media → equal except paths), `split_strips_proxy_and_local_paths` (no `/Users/` substring in the output JSON), `replace_sequence_only_touches_that_item`.
- [ ] **Step 2:** `cargo test -p filmcraft-team` fails. **Step 3:** Implement. **Step 4:** Passes. **Step 5:** Commit `feat(team): split and merge projects into shared and per-sequence parts`.

### Task F3: Change classification

**Files:** Create `crates/team/src/changes.rs`; tests in `crates/team/src/tests.rs`

**Interfaces:**
- Produces: `pub struct ChangeSet { pub sequences: BTreeSet<ItemId>, pub shared_items: BTreeSet<ItemId>, pub shared_other: bool /* bins, transcripts, source graphics, narrations, settings, notes */ }`; `pub fn classify(before: &Project, after: &Project) -> ChangeSet`. Compare sequence items with `Arc::ptr_eq` first, then by value; compare other items by value; removed or added items count as changed.

- [ ] **Step 1: Failing tests:** `edit_in_sequence_marks_only_that_sequence`, `import_marks_shared_item`, `bin_move_marks_shared_other`, `new_sequence_is_a_sequence_change`, `unchanged_is_empty`.
- [ ] Steps 2–5 as usual. Commit `feat(team): classify which parts of a project an edit changed`.

### Task F4: `filmcraft-drive` crate: client and sign-in

**Files:** Create `crates/drive/Cargo.toml` (deps: `ureq` with json, `serde`, `serde_json`, `keyring`, `sha2`, `base64`, `rand`), `crates/drive/src/{lib.rs,client.rs,auth.rs,tests.rs}`; add the deps to the workspace `Cargo.toml` `[workspace.dependencies]`.

**Interfaces:**
- Produces:
  - `pub struct DriveConfig { pub drive_url: String, pub lan_url: Option<String> }` and `DriveConfig::load() -> Result<Self>` (config file, then env `INFOCUS_DRIVE_URL`)
  - `pub fn login(cfg: &DriveConfig, open_browser: &dyn Fn(&str)) -> Result<String>`: PKCE against `/cli/authorize` and `POST /api/cli/token`, the same parameters as the Go CLI (`cli/internal/auth/login.go` in infocus-drive). The token is stored in Keychain service `InFocus Cut`, account `drive`.
  - `pub struct DriveClient` with one method per API route in Task D2 (`list_projects`, `create_project`, `get_project`, `take_lock`, `heartbeat`, `release`, `put_sequence`, `post_shared`, `keep`, `open`, `export_begin/end`), `events(project) -> impl Iterator<Item = TeamEvent>` (SSE line parser), and `download(drive_path, dest_partial: &Path, on_progress) -> Result<()>` (Range resume from the partial file's length).
  - The client picks `lan_url` when `GET {lan_url}/api/me` answers within 300 ms, else `drive_url`.
- [ ] **Step 1: Failing tests** against a `std::net::TcpListener` fake server in the test: `pkce_challenge_is_s256`, `take_lock_409_maps_to_held_by`, `put_426_maps_to_update_required`, `sse_parser_handles_split_chunks`, `resume_from_partial` (server honors Range; the final file equals the full body), `lan_preferred_when_reachable`.
- [ ] Steps 2–5. Commit `feat(drive): InFocus Drive client and sign-in`.

### Task F5: Team session in the engine

**Files:**
- Create: `crates/engine/src/team.rs`
- Modify: `crates/engine/src/lib.rs` (`Session`: `pub team: Option<team::TeamState>`; in `execute` after a journaled command succeeds, call `team::after_command(self, before)`; keep the `before` `Arc<Project>` clone already used for undo), `crates/engine/Cargo.toml` (dep `filmcraft-team`)
- Test: `crates/engine/src/team_tests.rs`

**Interfaces:**
- Consumes: F2 `split/replace_sequence/sequence_json`, F3 `classify`. Network stays out of the engine: `TeamState` talks to a `trait TeamLink { fn push_sequence(&self, id: ItemId, rev: u64, json: Value); fn push_shared(&self, upserts, deletes, bins); }` provided by the app (Task F7). Tests use a fake.
- Produces:
  - `pub struct TeamState { pub held: BTreeMap<ItemId, LockInfo>, pub holders: BTreeMap<ItemId, String>, pub revs: BTreeMap<ItemId, u64>, pub shared_rev: u64, link: Box<dyn TeamLink> }`
  - `team::after_command(s: &mut Session, before: Arc<Project>) -> Result<()>`: classify. If any changed sequence is not in `held`, restore `before`, drop the undo step just added, and return `EngineError::Disabled(id, format!("{holder} is editing “{name}”"))` (or "Take the lock to edit “{name}”" when it's free). Otherwise schedule a debounced (1 s) push for each held changed sequence and send shared changes at once.
  - `Session::apply_remote_sequence(&mut self, id: ItemId, rev: u64, json: &Value) -> Result<()>` and `apply_remote_shared(...)`: replace in the current project **and in every undo/redo snapshot** (so undo never rolls back a teammate's work), without adding an undo step.
  - `Session::lock_lost(&mut self, id: ItemId)`: if there are unpushed edits, copy the sequence as a new item "Recovered – <name>" (a new id from this member's slot), mark it unsynced, and remove `id` from `held`.
  - `Session::poll_team(&mut self, now: Instant)`: runs pending debounced pushes; called from `poll_persistence`.
- [ ] **Step 1: Failing tests:** `edit_touching_foreign_lock_reverts` (project and undo depth unchanged, error names the holder), `edit_in_held_sequence_pushes_after_debounce`, `import_pushes_shared_immediately`, `remote_update_survives_local_undo` (apply remote to seq B, undo a local edit in seq A → B keeps the remote version), `lost_lock_saves_recovered_copy`, `two_sessions_converge` (two Sessions wired through an in-memory fake service built on F2 and F3: A edits seq 1, B edits seq 2, both end with identical `split` output).
- [ ] Steps 2–5: `cargo test -p filmcraft-engine team`, then the whole `cargo test -p filmcraft-engine` (must stay green). Commit `feat(engine): team session with sequence locks and remote updates`.

### Task F6: Media cache and downloads

**Files:** Create `crates/team/src/cache.rs` (pure rules) and `crates/engine/src/team_media.rs` (download queue, which calls the existing proxy job); tests in each crate.

**Interfaces:**
- Produces in `filmcraft-team`:
  - `pub struct CacheIndex { entries: BTreeMap<Fingerprint, Entry { original: Option<(PathBuf, u64)>, proxy: Option<(PathBuf, u64)>, last_used: u64, projects: BTreeSet<String> }>, project_last_open: BTreeMap<String, u64> }` persisted as JSON at `<cache>/index.json`
  - `pub fn eviction_plan(idx: &CacheIndex, cap_bytes: u64, now: u64, in_use: &BTreeSet<Fingerprint>) -> Vec<PathBuf>`: originals first, oldest first, never proxies in `in_use`, plus everything that belongs only to projects not opened on this Mac for 30 days.
  - `pub fn wanted(project: &Project, open_sequences: &[ItemId], source: Option<ItemId>) -> BTreeSet<ItemId>` (media used by the open sequences, including nested ones, plus the source item)
- Produces in the engine: `TeamMedia::new(cache_dir, client: Arc<dyn Downloader>)`, `TeamMedia::ensure(&mut self, items: BTreeSet<ItemId>)`, `TeamMedia::poll(&mut self, s: &mut Session)` (when a download finishes: rename `.partial` → `original`, relink the item via `media.relink`, start a proxy with the existing proxies create command at ProRes 422 Proxy ½), `TeamMedia::pause(bool)`, `TeamMedia::progress() -> Vec<(ItemId, f32)>`. At most 2 downloads at once.
- [ ] **Step 1: Failing tests:** `wanted_includes_nested_sequence_media`, `eviction_originals_before_proxies`, `eviction_skips_in_use_proxy`, `thirty_day_project_evicted`, `partial_never_promoted` (a failed download leaves no `original`), `max_two_concurrent`, `paused_queue_starts_nothing`.
- [ ] Steps 2–5. Commit `feat(team): local media cache, on-demand downloads and proxies`.

### Task F7: App wiring and UI

**Files:**
- Create: `crates/ui-egui/src/panels/team.rs` (Team Projects panel: sign in, list, open, create from the current project, Keep toggle, media progress), `crates/ui-egui/src/team_link.rs` (implements `TeamLink` with `DriveClient` on a background thread; SSE thread feeds `apply_remote_*`; heartbeat thread every 20 s; heartbeat failure → `lock_lost`)
- Modify: the sequence tab drawing in `crates/ui-egui` (find with `grep -rn "open_sequences" crates/ui-egui/src/panels | head`) to show "Editing" / "Read-only: <name> is editing" / "Edit" (button takes the lock); the export entry point (find `export.start` or the Export dialog's run call) to call `TeamMedia` ensure-originals + `export_begin/end` first; the panel registry for the new panel; the app name and bundle id in `apps/filmcraft` packaging to "InFocus Cut" / `org.infocus.cut` *(builder: confirm where upstream sets the bundle id; `grep -rn "ai.storyteller.filmcraft" packaging apps | head`)*.
- Test: `crates/ui-egui` already has panel tests (`grep -rln "#\[test\]" crates/ui-egui/src | head`); add `team_panel_lists_projects_from_fake_client` and `tab_badge_text` following them.
- [ ] Steps 1–5. Build `nice -n 15 env CARGO_BUILD_JOBS=6 cargo build -p filmcraft --release`, launch it, and check that the Team panel opens and a sign-in attempt against an unreachable URL shows a clear error. Commit `feat(ui): team projects panel, lock badges and export guard`.

### Task F8: End-to-end check against a local Drive

**Files:** Create `scripts/team-e2e.sh` (starts infocus-drive's FastAPI app locally on a tmp drive root with a dev user, the same way `infocus-drive` tests/dev do it; runs two headless `filmcraft-cli` sessions with `--team` against it). Modify `apps/filmcraft-cli` to add `team open|lock|edit|push` subcommands used only by this script.
- [ ] The script: create a project from a demo `.fcproj`, A locks seq 1 and edits, B locks seq 2 and edits, B tries seq 1 (refused), A's heartbeat stops (the TTL is shortened by `TEAM_LOCK_TTL_S=3` in dev), B takes seq 1. It asserts both end with identical project JSON. Commit `test: two-member team editing end to end against a local Drive`.

---

## Not done overnight (needs the owner or 1Password)

- Deploying the infocus-drive changes to the NAS (needs NAS access).
- Signing and notarizing InFocus Cut (Apple signing items are in 1Password).
- The real two-Mac test on campus and off campus.
- Pushing infocus-drive to `main`: work goes to branch `feat/team-projects` with a PR for the owner to review.
