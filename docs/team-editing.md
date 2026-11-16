# Team editing (InFocus Cut fork)

InFocus Cut is a fork of FilmCraft that lets two or three members edit one project from their own
Macs. Each sequence has one editor at a time (a lock); everyone else sees it read-only and gets
updates live. Projects sync through InFocus Drive. The full design is in
[team-editing-design.md](team-editing-design.md).

## What the fork adds

- `crates/project`: per-member id slots (`Project::set_id_slot`, `ID_SLOT_SHIFT`), so two members
  never allocate the same item id. Slot 0 is the upstream behaviour.
- `crates/team` (`filmcraft-team`): pure logic with no network and no UI. It splits a project into a
  shared part and one part per sequence, merges them back, strips local paths and proxies, and
  classifies which parts an edit changed. `cache` holds the media cache rules: the index at
  `<cache>/index.json`, which media an editor needs (`wanted`: the open sequences, nested ones
  included, and the Source monitor), and what to remove (`eviction_plan`: a project's media 30
  days after it was last opened on this Mac; over 200 GB originals before proxies; never the
  media of an open sequence).
- `crates/drive` (`filmcraft-drive`, layer 5, no UI): the InFocus Drive client. Browser sign-in
  with PKCE (token in the Keychain, service `InFocus Cut`, account `drive`), one method per team
  project route, the change feed parser, resumable media downloads from the project's
  `<name> Media/` folder that only rename a `.partial` file once it is complete and checked, and
  resumable chunked media uploads into that folder that never overwrite (a name already there is
  `DriveError::NameTaken`). Lock requests name the Mac
  (`X-Drive-Device`), so a member's lock on one Mac is refused on their other Mac. Every team 409
  maps to its own `DriveError` variant by the server's `error` code. On campus it uses the LAN address with a short-lived LAN
  token (the long-lived token never crosses the LAN) and falls back to the internet address.
- `crates/team-link` (`filmcraft-team-link`, layer 5, no UI): connects a team session to the
  Drive. `DriveTeamLink` is the engine's `TeamLink`: requests run in order on a worker thread
  that keeps the lock tokens, heartbeats run on a thread of their own (so a slow save or upload
  can't hold one back past the 120 s lock expiry), and a feed thread follows the change feed, fetches only the
  sequence or shared part a teammate saved (`GET …/sequences/{seq}` and `GET …/shared` with
  `If-None-Match`, which don't count as opening the project), reconnects with backoff when the
  stream ends, and polls the whole project every 5 s while it is down (the whole project is
  also how it catches up after connecting, since only it lists sequences and locks). `DriveDownloader` is `TeamMedia`'s `Downloader`. `TeamUploads` uploads
  the member's own media (a project just made a team project, and anything imported into an open
  one) into `<name> Media/<bin path>/`, two at a time, paused during an export; a name a teammate
  already used gets a number (`A001 (2).mov`). The queue and session ids are in
  `<cache>/uploads.json`, so an upload cut by a quit resumes. Until it finishes the item plays
  from the member's file here and teammates see it as "uploading from <member>" (the shared item
  carries `local_only` and `uploading_by`); then the file goes into the cache index as an
  original that is never removed or downloaded again, and the item is sent again with its
  `drive:/` path. `TeamSessionController` is
  what the UI calls (sign in and out, list, create from the current project, open, keep, export
  guard); its network work runs on background threads and `poll` finishes it on the UI thread.
- Configuration: `~/Library/Application Support/InFocus Cut/config.toml` (see
  [`config/infocus.example.toml`](../config/infocus.example.toml)) or the build environment
  variable `INFOCUS_DRIVE_URL`. Hosts are never hard-coded.
- The ArtCraft brand marks are removed (`docs/brand/` logos, the README header and footer logos,
  and the wordmark in the About dialog and Home screen), as `docs/brand/LICENSE-brand.txt`
  requires of forks.

## Hooks into upstream code

Keep these small so upstream merges stay easy.

- Engine (`crates/engine/src/team.rs`): the field `Session::team` (None for an ordinary project,
  and then nothing below runs), two calls in `Session::execute` around the outermost command
  (`team::before_command` / `team::after_command`), one call to `Session::poll_team` at the top of
  `poll_persistence`, and the module declarations in `lib.rs`. The app connects through the
  `TeamLink` trait (requests return at once; outcomes come back from `TeamLink::poll`), so the
  engine never touches the network. Deleting a sequence needs its lock: the delete goes to the
  Drive once the pushes that stop other sequences nesting it are saved, and a sequence the Drive
  refuses to delete (another sequence on the Drive nests it, or the lock was lost) comes back.
  A teammate's delete removes the sequence here, from the undo history too. Project-level fields
  (settings, colours, LUTs, transcripts, search bins, source graphics, narrations and notes) are
  shared whole, the last writer winning per field; this Mac's scratch disks and ingest folder
  stay on it. The project name is not shared: it is the project's folder name on the Drive.
- Engine (`crates/engine/src/team_media.rs`): `TeamMedia` downloads originals on demand into
  `~/Library/Caches/InFocus Cut/media/<fingerprint>/original/` (at most two at once, paused
  during an export) through the app's `Downloader`, links them, and makes a ProRes 422 Proxy ½
  proxy in `…/proxy/` with the existing proxy job. These links stay on the Mac: they are written
  into the project and its undo snapshots without an undo step (`Session::patch_local`) and are
  never sent to the Drive. `TeamMedia::record_uploaded` and `relink_local` do the same for media
  this Mac uploads.
- Engine (`Session::team_push_items`): sends shared items again as they are, for a change only
  the link sees (an upload started or finished).
- UI (`crates/ui-egui`): the Team panel (`panels/team.rs`) draws a `TeamView` and sends
  `TeamAction`s through the object-safe `team_ui::TeamUi` trait. The desktop app plugs in
  `apps/filmcraft/src/team.rs` (over `TeamSessionController`) as `HostHooks::team`; without it (the
  web app) the panel is left out of the Window menu and nothing team-related runs. One call in
  `logic_background` (`team_ui::tick`) polls it every frame.
- Sequence tabs (`dock.rs`, `dock::tab_badge`): a badge after a team sequence's name: Editing
  (a dot while edits are not on the Drive yet), Requested, Read-only ("Maya is editing this",
  with a lock), or a Take button when the sequence is free. The Timeline's panel menu (also the
  tab's right-click menu) has Take Lock and Release Lock. A lock a member holds on their other
  Mac reads "You're editing this on <device>" (`team::editing_message`).
- Export (`team_ui::export_guard`, called by Export, Quick Export and Start Queue): in a team
  project the export waits, with a message naming what is still downloading, until the
  originals of the exported sequences are on this Mac (asking for them starts their download).
  `team_ui::tick` pauses downloads while any export runs, whichever route started it.
- Name: the app shows "InFocus Cut" (window title, macOS app menu, About, Quit/Hide, error and
  recovery dialogs) and the macOS bundle is `InFocus Cut.app` with the placeholder bundle id
  `org.infocus.cut` (`packaging/macos`). The About dialog says it is based on FilmCraft; the
  FilmCraft licence and copyright notices stay. The ArtCraft Discord and website links (Help
  menu, About, Home, the header's Discord button) are gone; GitHub and Report an Issue go to
  this fork. File formats keep their FilmCraft names (`.fcproj` is a FilmCraft Project).
- Updates and app data: Help ▸ Check for Updates (and the check at start) reads only this fork's
  GitHub releases (`filmcraft_ui_egui::UPDATE_APP`: id `infocus-cut`, repo
  `neelsatyavolu/filmcraft-infocus`, bundle id `org.infocus.cut`), never upstream FilmCraft's. A
  release it can install names its assets `infocus-cut-<version>-<os>-<arch>.<ext>` with a
  `SHA256SUMS.txt`; until there is one, a check finds nothing newer. On macOS the data folder
  (preferences, logs, recovery, models, and the Drive `config.toml`) is
  `~/Library/Application Support/InFocus Cut` (`autosave::MACOS_DATA_FOLDER`), fresh, never
  shared with or copied from an upstream FilmCraft install. The Keychain item is service
  `InFocus Cut`, account `drive`.

## Merging upstream

```sh
git fetch upstream
git switch -c merge/upstream-YYYY-MM-DD feat/team-editing
git merge upstream/main
```

Then check that no file in `docs/brand/` other than `LICENSE-brand.txt` came back, and that
`crates/ui-egui/src/brand.rs` is still gone. Run `cargo fmt --all -- --check`, clippy with
`-D warnings` and the tests for `filmcraft-project`, `filmcraft-team`, `filmcraft-engine` and
`filmcraft-ui-egui`, then open a pull request into the fork.

## Build time

The first release build of the clean fork (`nice -n 15 env CARGO_BUILD_JOBS=6 cargo build -p filmcraft
--release`, six jobs at low priority on a MacBook) took 4 min 43 s.
