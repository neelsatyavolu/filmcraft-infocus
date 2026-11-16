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
  resumable chunked media uploads into that folder. Lock requests name the Mac
  (`X-Drive-Device`), so a member's lock on one Mac is refused on their other Mac. Every team 409
  maps to its own `DriveError` variant by the server's `error` code. On campus it uses the LAN address with a short-lived LAN
  token (the long-lived token never crosses the LAN) and falls back to the internet address.
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
  engine never touches the network. Limits for now: deleting a sequence is refused in a team
  project, and only items and bins are shared (settings, transcripts, source graphics and notes
  stay on each Mac), because the Drive API has no route for them.
- Engine (`crates/engine/src/team_media.rs`): `TeamMedia` downloads originals on demand into
  `~/Library/Caches/InFocus Cut/media/<fingerprint>/original/` (at most two at once, paused
  during an export) through the app's `Downloader`, links them, and makes a ProRes 422 Proxy ½
  proxy in `…/proxy/` with the existing proxy job. These links stay on the Mac: they are written
  into the project and its undo snapshots without an undo step (`Session::patch_local`) and are
  never sent to the Drive.
- UI (planned): a Team Projects panel, lock badges on sequence tabs, and a guard on the export
  entry point that fetches originals first.
- Packaging (planned): app name "InFocus Cut" and its own bundle id.

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
