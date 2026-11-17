#!/usr/bin/env bash
# End-to-end check of team editing: three scripted members (`filmcraft-cli team session`) against a
# real InFocus Drive server running on 127.0.0.1 (the Drive's dev-only launcher,
# scripts/dev/team_e2e_server.py, with fictional members student1-3 and the fictional admin
# teacher1). Footage is generated with ffmpeg (1080p H.264 and ProRes). The editing dashboard's
# upload and the non-member / admin checks go through scripts/team-e2e-web.py. No NAS, no network
# beyond loopback.
#
# The story: student1 creates the project with footage, adds one more file through the dashboard,
# invites student2 and student3; student3 declines and cannot see the project, the admin can;
# student2 joins, opens a sequence, the footage arrives intact and gets a local proxy; student2's
# new footage reaches student1; concurrent edits converge; locks refuse a second editor; a lost lock
# is recovered; student2 leaves; the last member cannot leave. Download and upload speeds print.
#
#   scripts/team-e2e.sh
#
# Environment:
#   INFOCUS_DRIVE_REPO    infocus-drive checkout (default: ../infocus-drive next to this repo)
#   INFOCUS_DRIVE_PYTHON  its Python (default: $INFOCUS_DRIVE_REPO/.venv/bin/python)
#   TEAM_E2E_LOCK_TTL_S   the Drive's lock expiry for the run (default 25; heartbeats are every 20 s)
#   TEAM_E2E_KEEP=1       keep the temp folder (logs, member folders, the Drive root)
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
DRIVE_REPO=${INFOCUS_DRIVE_REPO:-$ROOT/../infocus-drive}
PY=${INFOCUS_DRIVE_PYTHON:-$DRIVE_REPO/.venv/bin/python}
TTL=${TEAM_E2E_LOCK_TTL_S:-25}
PORT=${TEAM_E2E_PORT:-$((20000 + RANDOM % 20000))}
WORK=$(mktemp -d "${TMPDIR:-/tmp}/team-e2e.XXXXXX")
CLI=$ROOT/target/debug/filmcraft-cli
WEB=$ROOT/scripts/team-e2e-web.py
SERVER_PID=""
M1_PID=""
M2_PID=""
M3_PID=""
STEP="setup"

say() { printf '\n== %s\n' "$*"; }
die() {
  printf '\nFAILED at step "%s": %s\n' "$STEP" "$*" >&2
  for f in "$WORK"/m1.err "$WORK"/m2.err "$WORK"/m3.err "$WORK"/server.log; do
    [ -s "$f" ] && { printf -- '--- last lines of %s\n' "$f" >&2; tail -n 15 "$f" >&2; }
  done
  exit 1
}

cleanup() {
  local code=$?
  exec 3>&- 4<&- 5>&- 6<&- 7>&- 8<&- 2>/dev/null || true
  for pid in "$M1_PID" "$M2_PID" "$M3_PID" "$SERVER_PID"; do
    [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  done
  wait 2>/dev/null || true
  if [ "${TEAM_E2E_KEEP:-}" = 1 ]; then
    echo "kept $WORK"
  else
    rm -rf "$WORK"
  fi
  exit "$code"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

[ -f "$DRIVE_REPO/scripts/dev/team_e2e_server.py" ] || die "no infocus-drive checkout with scripts/dev/team_e2e_server.py at $DRIVE_REPO (set INFOCUS_DRIVE_REPO)"
[ -x "$PY" ] || die "no Python at $PY (set INFOCUS_DRIVE_PYTHON)"
command -v ffmpeg >/dev/null 2>&1 || die "ffmpeg is needed to generate the test footage"

# --- JSON helpers -------------------------------------------------------------------------------

# jq-free: evaluates a Python expression over `d` (the JSON in $1).
jget() { "$PY" -c 'import json,sys; d=json.loads(sys.argv[1]); r=eval(sys.argv[2]); print(json.dumps(r) if isinstance(r,(dict,list)) else r)' "$1" "$2"; }

REPLY_JSON=""
# ask <member 1|2|3> <command line>: sends one session command and reads its one-line answer.
ask() {
  local m=$1 line="" in out
  shift
  case "$m" in 1) in=3 out=4 ;; 2) in=5 out=6 ;; 3) in=7 out=8 ;; *) die "no member $m" ;; esac
  echo "$*" >&"$in"
  IFS= read -r -t 300 -u "$out" line || die "student$m gave no answer to: $*"
  REPLY_JSON=$line
}
# ok <member> <command>: the command must succeed.
ok() {
  ask "$@"
  [ "$(jget "$REPLY_JSON" 'd["ok"]')" = True ] || die "student$1 '${*:2}' failed: $(jget "$REPLY_JSON" 'd.get("error")')"
}
# refused <member> <command>: the command must fail; its error is in $REFUSAL.
refused() {
  ask "$@"
  [ "$(jget "$REPLY_JSON" 'd["ok"]')" = False ] || die "student$1 '${*:2}' should have been refused: $REPLY_JSON"
  REFUSAL=$(jget "$REPLY_JSON" 'd.get("error")')
}
now() { "$PY" -c 'import time; print(time.time())'; }
# mb_s <bytes> <start> <end>
mb_s() { "$PY" -c 'import sys; b,a,z=map(float,sys.argv[1:]); print(round(b/1e6/max(z-a,1e-6),1))' "$@"; }
size_of() { stat -f %z "$1"; }
sha() { shasum -a 256 "$1" | cut -d' ' -f1; }
# http <who: user name | @token file> <path>: one GET; prints {"status", "body"}.
http() { "$PY" "$WEB" get "$URL" "$1" "$2"; }
status_of() { jget "$1" 'd["status"]'; }
result() { jget "$REPLY_JSON" "$1"; }
seq_id() { result "[s['id'] for s in d['result']['sequences'] if s['name']=='$1'][0]"; }
media_of() { result "[m['$2'] for m in d['result']['media'] if m['name']=='$1'][0]"; }
check() { [ "$(result "$1")" = True ] || die "$2"; }

same_split() {
  ok 1 split; local a=$REPLY_JSON
  ok 2 split; local b=$REPLY_JSON
  jget "$a" 'd["result"]' >"$WORK/split1.json"
  jget "$b" 'd["result"]' >"$WORK/split2.json"
  if ! "$PY" -c 'import json,sys; a,b=(json.load(open(p)) for p in sys.argv[1:]); sys.exit(a!=b)' "$WORK/split1.json" "$WORK/split2.json"; then
    "$PY" -c 'import json,sys; [json.dump(json.load(open(p)),open(p+".pretty","w"),indent=1,sort_keys=True) for p in sys.argv[1:]]' "$WORK/split1.json" "$WORK/split2.json"
    diff -u "$WORK/split1.json.pretty" "$WORK/split2.json.pretty" | head -n 40 >&2 || true
    die "the members' projects differ ($1)"
  fi
  echo "split identical ($1)"
}
sync_both() { ok 1 sync; ok 2 sync; ok 1 sync; ok 2 sync; }

# --- build, media, Drive ------------------------------------------------------------------------

STEP="build"
say "building filmcraft-cli"
(cd "$ROOT" && nice -n 15 env CARGO_BUILD_JOBS=6 cargo build -q -p filmcraft-cli) || die "cargo build failed"

STEP="media"
say "generating footage (1080p H.264 and ProRes, with a tone)"
# footage <file> <seconds> <tone Hz> <source> <codec args…>
footage() {
  local out=$1 secs=$2 hz=$3 src=$4
  shift 4
  ffmpeg -loglevel error -y -f lavfi -i "$src=size=1920x1080:rate=30:duration=$secs" -f lavfi -i "sine=frequency=$hz:duration=$secs" \
    "$@" -shortest "$out" || die "ffmpeg could not make $out"
}
H264=(-c:v libx264 -preset veryfast -pix_fmt yuv420p -c:a aac)
PRORES=(-c:v prores_ks -profile:v 0 -pix_fmt yuv422p10le -c:a pcm_s16le)
F=$WORK/footage
mkdir -p "$F"
footage "$F/A001_interview.mp4" 12 440 testsrc2 "${H264[@]}"
footage "$F/B002_broll.mp4" 10 550 smptebars "${H264[@]}"
footage "$F/C003_wide.mov" 6 330 testsrc2 "${PRORES[@]}"
footage "$F/D004_web.mp4" 8 660 testsrc2 "${H264[@]}"
footage "$F/E005_student2.mov" 5 770 testsrc2 "${PRORES[@]}"
footage "$F/from1.mp4" 1 550 testsrc2 "${H264[@]}"
footage "$F/from2.mp4" 1 660 smptebars "${H264[@]}"
ls -l "$F" | awk 'NR>1 {printf "  %-22s %6.1f MB\n", $9, $5/1e6}'

STEP="project"
"$CLI" --compact exec file.newProject '{"name":"E2E"}' --save-as "$WORK/start.fcproj" >/dev/null
"$CLI" --compact --project "$WORK/start.fcproj" import "$F/A001_interview.mp4" "$F/B002_broll.mp4" "$F/C003_wide.mov" --save >/dev/null
printf '%s\n' \
  '{"id":"file.newSequenceFromClip","params":{"items":[1]}}' \
  '{"id":"file.newSequenceFromClip","params":{"items":[2]}}' |
  "$CLI" --compact --project "$WORK/start.fcproj" run - --save >"$WORK/seqs.jsonl"
A0=$(head -n 1 "$WORK/seqs.jsonl" | "$PY" -c 'import json,sys; print(json.load(sys.stdin)["result"]["sequence"])')
B0=$(tail -n 1 "$WORK/seqs.jsonl" | "$PY" -c 'import json,sys; print(json.load(sys.stdin)["result"]["sequence"])')
printf '%s\n' "{\"id\":\"clip.rename\",\"params\":{\"item\":$A0,\"name\":\"Seq A\"}}" "{\"id\":\"clip.rename\",\"params\":{\"item\":$B0,\"name\":\"Seq B\"}}" |
  "$CLI" --compact --project "$WORK/start.fcproj" run - --save >/dev/null
UP_BYTES=$(( $(size_of "$F/A001_interview.mp4") + $(size_of "$F/B002_broll.mp4") + $(size_of "$F/C003_wide.mov") ))

STEP="drive"
say "starting the local Drive on 127.0.0.1:$PORT (lock expiry ${TTL}s)"
INFOCUS_DEV_E2E=1 INFOCUS_DEV_TEAM_LOCK_TTL_S=$TTL "$PY" "$DRIVE_REPO/scripts/dev/team_e2e_server.py" \
  --dir "$WORK/drive" --port "$PORT" >"$WORK/server.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 100); do grep -q '^READY' "$WORK/server.log" 2>/dev/null && break; kill -0 "$SERVER_PID" 2>/dev/null || die "the Drive exited"; sleep 0.2; done
grep -q '^READY' "$WORK/server.log" || die "the Drive did not start"
URL="http://127.0.0.1:$PORT"
MEDIA_DIR="$WORK/drive/drive/Team Projects/E2E Media"

STEP="login"
for n in 1 2 3; do
  "$CLI" team login --dir "$WORK/m$n" --drive "$URL" --token-file "$WORK/drive/student$n.token" --device "e2e-mac-$n" >/dev/null || die "student$n could not sign in"
done

mkfifo "$WORK/in1" "$WORK/out1" "$WORK/in2" "$WORK/out2" "$WORK/in3" "$WORK/out3"
"$CLI" team session --dir "$WORK/m1" <"$WORK/in1" >"$WORK/out1" 2>"$WORK/m1.err" &
M1_PID=$!
exec 3>"$WORK/in1" 4<"$WORK/out1"
"$CLI" team session --dir "$WORK/m2" <"$WORK/in2" >"$WORK/out2" 2>"$WORK/m2.err" &
M2_PID=$!
exec 5>"$WORK/in2" 6<"$WORK/out2"
"$CLI" team session --dir "$WORK/m3" <"$WORK/in3" >"$WORK/out3" 2>"$WORK/m3.err" &
M3_PID=$!
exec 7>"$WORK/in3" 8<"$WORK/out3"
for fd in 4 6 8; do IFS= read -r -t 60 -u "$fd" REPLY_JSON || die "a member's session did not start"; done

# --- the scenario -------------------------------------------------------------------------------

STEP="create and upload"
say "student1 creates the project with three clips (uploads them)"
T0=$(now)
ok 1 create E2E "$WORK/start.fcproj"
T1=$(now)
ID=$(result 'd["result"]["id"]')
ok 1 status
check 'd["result"]["upload_failures"] == []' "student1's uploads failed: $(result 'd["result"]["upload_failures"]')"
for f in A001_interview.mp4 B002_broll.mp4 C003_wide.mov; do
  cmp -s "$MEDIA_DIR/$f" "$F/$f" || die "$f on the Drive differs from the original"
done
UPLOAD_APP=$(mb_s "$UP_BYTES" "$T0" "$T1")
echo "project $ID; $UP_BYTES bytes uploaded by the app at $UPLOAD_APP MB/s (create included)"

STEP="dashboard upload"
say "student1 adds one more file through the editing dashboard's upload"
WEB_UP=$("$PY" "$WEB" upload "$URL" student1 "$ID" "$F/D004_web.mp4") || die "the dashboard upload failed"
UPLOAD_WEB=$(jget "$WEB_UP" 'd["mb_s"]')
cmp -s "$MEDIA_DIR/D004_web.mp4" "$F/D004_web.mp4" || die "the dashboard's file on the Drive differs from the original"
R=$(http @"$WORK/drive/student1.token" "/api/team/projects/$ID/media")
[ "$(jget "$R" 'sorted(f["path"] for f in d["body"]["files"])')" = '["A001_interview.mp4", "B002_broll.mp4", "C003_wide.mov", "D004_web.mp4"]' ] ||
  die "the footage list is not the four files: $R"
echo "dashboard upload at $UPLOAD_WEB MB/s; footage list has all four files"

STEP="invite"
say "student1 invites student2 and student3; student3 declines"
refused 2 open "$ID"
ok 1 invite "$ID" student2 student3
for n in 2 3; do
  ok "$n" list
  check '[p["role"] for p in d["result"] if p["id"]=="'"$ID"'"] == ["invited"]' "student$n does not see the invitation: $(result 'd["result"]')"
  check '[p["invited_by"] for p in d["result"] if p["id"]=="'"$ID"'"] == ["Student One"]' "student$n does not see who invited them: $(result 'd["result"]')"
done
ok 3 decline "$ID"
ok 3 list
check '[p for p in d["result"] if p["id"]=="'"$ID"'"] == []' "student3 still sees the project after declining"

STEP="who can see it"
say "a non-member cannot see or fetch the project; the admin can see it"
refused 3 open "$ID"
for path in "/api/team/projects/$ID/overview" "/api/team/projects/$ID/media" "/api/team/projects/$ID/media?path=A001_interview.mp4" "/api/team/projects/$ID" "/api/team/projects/$ID/shared"; do
  R=$(http @"$WORK/drive/student3.token" "$path")
  [ "$(status_of "$R")" = 404 ] || die "student3 got $path: $R"
done
R=$(http @"$WORK/drive/student3.token" "/api/team/projects?all=1")
[ "$(jget "$R" '[p for p in d["body"] if p["id"]=="'"$ID"'"]')" = "[]" ] || die "student3's list shows the project: $R"
R=$(http teacher1 "/api/team/projects?all=1")
[ "$(jget "$R" '[p["role"] for p in d["body"] if p["id"]=="'"$ID"'"]')" = '["admin"]' ] || die "the admin does not see the project: $R"
R=$(http teacher1 "/api/team/projects/$ID/overview")
[ "$(status_of "$R")" = 200 ] || die "the admin cannot read the overview: $R"
R=$(http teacher1 "/api/team/projects")
[ "$(jget "$R" '[p for p in d["body"] if p["id"]=="'"$ID"'"]')" = "[]" ] || die "the admin's own list shows a project they are not in: $R"
echo "student3: 404 on the overview, footage list, download and project; the admin sees it in All projects"

STEP="join and download"
say "student2 joins, opens Seq A; the footage downloads and gets a local proxy"
ok 2 join "$ID"
SA=$(seq_id "Seq A"); SB=$(seq_id "Seq B")
T0=$(now)
ok 2 exec sequence.open "{\"item\":$SA}"
ok 2 local
T1=$(now)
P2=$(media_of A001_interview.mp4 path)
case "$P2" in "$WORK/m2/cache/"*) ;; *) die "student2's A001 is not in its media cache: $P2" ;; esac
[ "$(sha "$P2")" = "$(sha "$F/A001_interview.mp4")" ] || die "student2's A001 checksum differs from the original"
DOWN_APP=$(mb_s "$(size_of "$P2")" "$T0" "$T1")
[ "$(media_of B002_broll.mp4 path)" = "drive:/Team Projects/E2E Media/B002_broll.mp4" ] || die "student2 downloaded B002 although no open sequence uses it"
ok 2 local proxies
PX=$(media_of A001_interview.mp4 proxy)
case "$PX" in "$WORK/m2/cache/"*/proxy/*) [ -s "$PX" ] || die "student2's proxy file is empty: $PX" ;; *) die "student2 has no local proxy for A001: $PX" ;; esac
echo "A001 downloaded at $DOWN_APP MB/s (sha256 match); proxy made on student2's Mac: ${PX#"$WORK"/}"
T0=$(now)
ok 2 fetch
T1=$(now)
P2B=$(media_of B002_broll.mp4 path)
[ "$(sha "$P2B")" = "$(sha "$F/B002_broll.mp4")" ] || die "student2's B002 checksum differs from the original"
echo "B002 (the other sequence's footage) downloaded at $(mb_s "$(size_of "$P2B")" "$T0" "$T1") MB/s (sha256 match)"

STEP="website footage"
say "student1's Mac picks up the file added on the dashboard"
ok 1 drive-footage
check '"D004_web.mp4" in d["result"]["added"]' "student1 was not offered the dashboard file: $(result 'd["result"]')"
ok 1 status
P1D=$(media_of D004_web.mp4 path)
[ "$(sha "$P1D")" = "$(sha "$F/D004_web.mp4")" ] || die "student1's copy of the dashboard file differs"
sync_both
ok 2 status
[ "$(media_of D004_web.mp4 path)" = "drive:/Team Projects/E2E Media/D004_web.mp4" ] || die "student2 does not have the dashboard file as a Drive item: $(result 'd["result"]["media"]')"
echo "D004 is in both members' bins (student1 has it locally)"

STEP="locks"
say "student1 locks and edits Seq A; student2 is refused it, and edits Seq B"
ok 1 lock "$SA"
ok 1 edit "$SA" "Seq A by student1"
refused 2 lock "$SA"
case "$REFUSAL" in *student1*) echo "student2: $REFUSAL" ;; *) die "refusal does not name the holder: $REFUSAL" ;; esac
ok 2 lock "$SB"
ok 2 edit "$SB" "Seq B by student2"
sync_both
ok 1 status
check '[s["name"] for s in d["result"]["sequences"] if s["id"]=='"$SB"'] == ["Seq B by student2"]' "student1 does not see student2's edit"
ok 2 status
check '[s["name"] for s in d["result"]["sequences"] if s["id"]=='"$SA"'] == ["Seq A by student1"]' "student2 does not see student1's edit"
check '[s["holder"] for s in d["result"]["sequences"] if s["id"]=='"$SA"'] == ["student1"]' "student2 does not see student1's lock"
same_split "after concurrent edits"

STEP="new footage from student2"
say "student2 adds new footage; student1's Mac picks it up and downloads it"
T0=$(now)
ok 2 import "$F/E005_student2.mov"
T1=$(now)
UPLOAD_M2=$(mb_s "$(size_of "$F/E005_student2.mov")" "$T0" "$T1")
cmp -s "$MEDIA_DIR/E005_student2.mov" "$F/E005_student2.mov" || die "student2's new footage on the Drive differs"
sync_both
ok 1 status
E5=$(media_of E005_student2.mov id)
[ "$(media_of E005_student2.mov path)" = "drive:/Team Projects/E2E Media/E005_student2.mov" ] || die "student1 does not have student2's new footage: $(result 'd["result"]["media"]')"
T0=$(now)
ok 1 exec source.open "{\"item\":$E5}"
ok 1 local proxies
T1=$(now)
P1E=$(media_of E005_student2.mov path)
[ "$(sha "$P1E")" = "$(sha "$F/E005_student2.mov")" ] || die "student1's copy of student2's footage differs"
[ -s "$(media_of E005_student2.mov proxy)" ] || die "student1 made no proxy for student2's footage"
echo "student2 uploaded E005 at $UPLOAD_M2 MB/s; student1 downloaded it (sha256 match) and made its proxy in $("$PY" -c 'import sys; print(round(float(sys.argv[2])-float(sys.argv[1]),1))' "$T0" "$T1") s"

STEP="imports"
say "both import media at the same moment"
echo "import $F/from1.mp4" >&3
echo "import $F/from2.mp4" >&5
IFS= read -r -t 180 R1 <&4 || die "student1 gave no answer to import"
IFS= read -r -t 180 R2 <&6 || die "student2 gave no answer to import"
[ "$(jget "$R1" 'd["ok"]')" = True ] || die "student1's import failed: $R1"
[ "$(jget "$R2" 'd["ok"]')" = True ] || die "student2's import failed: $R2"
sync_both
for n in 1 2; do
  ok "$n" status
  check '{"from1.mp4","from2.mp4"} <= {m["name"] for m in d["result"]["media"] if m["in_bin"]}' \
    "student$n's bins lack an import: $(result '[m["name"] for m in d["result"]["media"]]')"
  check 'd["result"]["upload_failures"] == []' "student$n has upload failures: $(result 'd["result"]["upload_failures"]')"
done
[ -f "$MEDIA_DIR/from1.mp4" ] || die "from1 is not in the project's media folder on the Drive"
[ -f "$MEDIA_DIR/from2.mp4" ] || die "from2 is not in the project's media folder on the Drive"
same_split "after concurrent imports"

STEP="lost lock"
say "student1 edits Seq A and sleeps past the lock expiry; student2 takes Seq A over"
ok 1 edit "$SA" "Seq A before sleep"
echo "freeze $((TTL + 15))" >&3
sleep $((TTL + 3))
GOT=""
for _ in 1 2 3 4 5; do
  ask 2 lock "$SA"
  [ "$(result 'd["ok"]')" = True ] && { GOT=1; break; }
  sleep 1
done
[ -n "$GOT" ] || die "student2 could not take Seq A after student1's lock expired: $REPLY_JSON"
ok 2 edit "$SA" "Seq A by student2"
ok 2 sync
IFS= read -r -t $((TTL + 60)) REPLY_JSON <&4 || die "student1 did not wake up"
sync_both
ok 1 status
check '[s for s in d["result"]["sequences"] if s["name"].startswith("Recovered") and "Seq A before sleep" in s["name"]] != []' \
  "student1's unsynced edit was not saved as a Recovered copy: $(result '[s["name"] for s in d["result"]["sequences"]]')"
check '[s["name"] for s in d["result"]["sequences"] if s["id"]=='"$SA"'] == ["Seq A by student2"]' "student1 does not have student2's Seq A"
check '[s["status"] for s in d["result"]["sequences"] if s["id"]=='"$SA"'] == ["read-only"]' "student1 still edits Seq A"
echo "student1: $(result '[m for m in d["result"]["messages"] if "lost the lock" in m]')"
ok 2 status
check '[s for s in d["result"]["sequences"] if s["name"].startswith("Recovered")] != []' "student2 does not see the Recovered copy"
same_split "after the lost lock"

STEP="delete"
say "student2 deletes Seq B"
ok 2 delete "$SB"
sync_both
for n in 1 2; do
  ok "$n" status
  check '[s for s in d["result"]["sequences"] if s["id"]=='"$SB"'] == []' "student$n still has Seq B"
done
same_split "after the delete"

STEP="leave"
say "student2 leaves; student1, the last member, cannot"
ok 2 sync
ok 2 leave "$ID"
ok 2 list
check '[p for p in d["result"] if p["id"]=="'"$ID"'"] == []' "student2 still sees the project after leaving"
R=$(http @"$WORK/drive/student2.token" "/api/team/projects/$ID/overview")
[ "$(status_of "$R")" = 404 ] || die "student2 can still read the project after leaving: $R"
refused 1 leave "$ID"
case "$REFUSAL" in *last*) echo "student1: $REFUSAL" ;; *) die "the last member's refusal does not say why: $REFUSAL" ;; esac
R=$(http @"$WORK/drive/student1.token" "/api/team/projects/$ID/overview")
[ "$(jget "$R" '[m["username"] for m in d["body"]["members"]]')" = '["student1"]' ] || die "the members are not just student1: $R"

STEP="quit"
for fd in 3 5 7; do echo quit >&"$fd"; done
wait "$M1_PID" || die "student1's session exited with an error"
wait "$M2_PID" || die "student2's session exited with an error"
wait "$M3_PID" || die "student3's session exited with an error"
M1_PID=""
M2_PID=""
M3_PID=""
STEP="bench"
say "transfer speed on a bigger file (team bench: one stream against parallel ranges and chunks)"
footage "$F/F006_bench.mov" 30 880 testsrc2 "${PRORES[@]}"
BENCH=$("$CLI" --compact team bench --dir "$WORK/m1" "$ID" "$F/F006_bench.mov") || die "team bench failed"
BENCH_LINE=$(jget "$BENCH" '"; ".join("%s: up %s MB/s, down %s MB/s" % (r["mode"], r["upload_mb_s"], r["download_mb_s"]) for r in d["runs"])')
echo "$(jget "$BENCH" 'round(d["bytes"]/1e6,1)') MB file (sha256 checked): $BENCH_LINE"

say "speeds over loopback: app upload $UPLOAD_APP MB/s (3 clips, create included), dashboard upload $UPLOAD_WEB MB/s, app download $DOWN_APP MB/s (A001), student2's upload $UPLOAD_M2 MB/s; bench $BENCH_LINE"
say "PASSED: team editing end to end against a local Drive (three members)"
