#!/usr/bin/env bash
# End-to-end check of team editing: two scripted members (`filmcraft-cli team session`) against a
# real InFocus Drive server running on 127.0.0.1 (the Drive's dev-only launcher,
# scripts/dev/team_e2e_server.py, with fictional members student1 and student2). No NAS, no network
# beyond loopback.
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
SERVER_PID=""
M1_PID=""
M2_PID=""
STEP="setup"

say() { printf '\n== %s\n' "$*"; }
die() {
  printf '\nFAILED at step "%s": %s\n' "$STEP" "$*" >&2
  for f in "$WORK"/m1.err "$WORK"/m2.err "$WORK"/server.log; do
    [ -s "$f" ] && { printf -- '--- last lines of %s\n' "$f" >&2; tail -n 15 "$f" >&2; }
  done
  exit 1
}

cleanup() {
  local code=$?
  exec 3>&- 4<&- 5>&- 6<&- 2>/dev/null || true
  for pid in "$M1_PID" "$M2_PID" "$SERVER_PID"; do
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

# --- JSON helpers -------------------------------------------------------------------------------

# jq-free: evaluates a Python expression over `d` (the JSON in $1).
jget() { "$PY" -c 'import json,sys; d=json.loads(sys.argv[1]); r=eval(sys.argv[2]); print(json.dumps(r) if isinstance(r,(dict,list)) else r)' "$1" "$2"; }

REPLY_JSON=""
# ask <member 1|2> <command line>: sends one session command and reads its one-line answer.
ask() {
  local m=$1 line=""
  shift
  if [ "$m" = 1 ]; then echo "$*" >&3; IFS= read -r -t 180 line <&4 || die "student1 gave no answer to: $*"
  else echo "$*" >&5; IFS= read -r -t 180 line <&6 || die "student2 gave no answer to: $*"; fi
  REPLY_JSON=$line
}
# ok <member> <command>: the command must succeed.
ok() {
  ask "$@"
  [ "$(jget "$REPLY_JSON" 'd["ok"]')" = True ] || die "student$1 '${*:2}' failed: $(jget "$REPLY_JSON" 'd.get("error")')"
}
result() { jget "$REPLY_JSON" "$1"; }
seq_id() { result "[s['id'] for s in d['result']['sequences'] if s['name']=='$1'][0]"; }
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
make_media() { # <file> <seconds> <tone Hz>
  if command -v ffmpeg >/dev/null 2>&1; then
    ffmpeg -loglevel error -y -f lavfi -i "testsrc=size=160x90:rate=24:duration=$2" -f lavfi -i "sine=frequency=$3:duration=$2" \
      -c:v libx264 -pix_fmt yuv420p -c:a aac -shortest "$1.mp4" && mv "$1.mp4" "$1"
  else
    "$PY" - "$1" "$2" "$3" <<'EOF'
import math, struct, sys, wave
path, secs, hz = sys.argv[1], float(sys.argv[2]), float(sys.argv[3])
with wave.open(path, "wb") as w:
    w.setnchannels(1); w.setsampwidth(2); w.setframerate(48000)
    w.writeframes(b"".join(struct.pack("<h", int(8000 * math.sin(2 * math.pi * hz * i / 48000))) for i in range(int(48000 * secs))))
EOF
  fi
}
EXT=mp4; command -v ffmpeg >/dev/null 2>&1 || EXT=wav
make_media "$WORK/clip.$EXT" 2 440
make_media "$WORK/from1.$EXT" 1 550
make_media "$WORK/from2.$EXT" 1 660

STEP="project"
"$CLI" --compact exec file.newProject '{"name":"E2E"}' --save-as "$WORK/start.fcproj" >/dev/null
"$CLI" --compact --project "$WORK/start.fcproj" import "$WORK/clip.$EXT" --save >/dev/null
printf '%s\n' \
  '{"id":"file.newSequenceFromClip","params":{"items":[1]}}' \
  '{"id":"file.newSequenceFromClip","params":{"items":[1]}}' |
  "$CLI" --compact --project "$WORK/start.fcproj" run - --save >"$WORK/seqs.jsonl"
A0=$(head -n 1 "$WORK/seqs.jsonl" | "$PY" -c 'import json,sys; print(json.load(sys.stdin)["result"]["sequence"])')
B0=$(tail -n 1 "$WORK/seqs.jsonl" | "$PY" -c 'import json,sys; print(json.load(sys.stdin)["result"]["sequence"])')
printf '%s\n' "{\"id\":\"clip.rename\",\"params\":{\"item\":$A0,\"name\":\"Seq A\"}}" "{\"id\":\"clip.rename\",\"params\":{\"item\":$B0,\"name\":\"Seq B\"}}" |
  "$CLI" --compact --project "$WORK/start.fcproj" run - --save >/dev/null

STEP="drive"
say "starting the local Drive on 127.0.0.1:$PORT (lock expiry ${TTL}s)"
INFOCUS_DEV_E2E=1 INFOCUS_DEV_TEAM_LOCK_TTL_S=$TTL "$PY" "$DRIVE_REPO/scripts/dev/team_e2e_server.py" \
  --dir "$WORK/drive" --port "$PORT" >"$WORK/server.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 100); do grep -q '^READY' "$WORK/server.log" 2>/dev/null && break; kill -0 "$SERVER_PID" 2>/dev/null || die "the Drive exited"; sleep 0.2; done
grep -q '^READY' "$WORK/server.log" || die "the Drive did not start"
URL="http://127.0.0.1:$PORT"

STEP="login"
for n in 1 2; do
  "$CLI" team login --dir "$WORK/m$n" --drive "$URL" --token-file "$WORK/drive/student$n.token" --device "e2e-mac-$n" >/dev/null || die "student$n could not sign in"
done

mkfifo "$WORK/in1" "$WORK/out1" "$WORK/in2" "$WORK/out2"
"$CLI" team session --dir "$WORK/m1" <"$WORK/in1" >"$WORK/out1" 2>"$WORK/m1.err" &
M1_PID=$!
exec 3>"$WORK/in1" 4<"$WORK/out1"
"$CLI" team session --dir "$WORK/m2" <"$WORK/in2" >"$WORK/out2" 2>"$WORK/m2.err" &
M2_PID=$!
exec 5>"$WORK/in2" 6<"$WORK/out2"
IFS= read -r -t 60 REPLY_JSON <&4 || die "student1's session did not start"
IFS= read -r -t 60 REPLY_JSON <&6 || die "student2's session did not start"

# --- the scenario -------------------------------------------------------------------------------

STEP="create and open"
say "student1 creates the project; student2 opens it and downloads the media"
ok 1 create E2E "$WORK/start.fcproj"
ID=$(result 'd["result"]["id"]')
ok 2 open "$ID"
SA=$(seq_id "Seq A"); SB=$(seq_id "Seq B")
ok 2 fetch
check '[m for m in d["result"]["media"] if m["name"]=="clip.'"$EXT"'" and m["path"] and m["path"].startswith("'"$WORK/m2/cache"'")] != []' \
  "student2's clip is not in its media cache: $(result 'd["result"]["media"]')"
P2=$(result '[m["path"] for m in d["result"]["media"] if m["name"]=="clip.'"$EXT"'"][0]')
cmp -s "$P2" "$WORK/clip.$EXT" || die "student2's downloaded clip differs from the original"
echo "project $ID, Seq A=$SA, Seq B=$SB; student2 downloaded the clip intact"

STEP="locks"
say "student1 locks and edits Seq A; student2 is refused it, and edits Seq B"
ok 1 lock "$SA"
ok 1 edit "$SA" "Seq A by student1"
ask 2 lock "$SA"
[ "$(result 'd["ok"]')" = False ] || die "student2 got the lock student1 holds"
case "$(result 'd["error"]')" in *student1*) echo "student2 $(result 'd["error"]')" ;; *) die "refusal does not name the holder: $REPLY_JSON" ;; esac
ok 2 lock "$SB"
ok 2 edit "$SB" "Seq B by student2"
sync_both
ok 1 status
check '[s["name"] for s in d["result"]["sequences"] if s["id"]=='"$SB"'] == ["Seq B by student2"]' "student1 does not see student2's edit"
ok 2 status
check '[s["name"] for s in d["result"]["sequences"] if s["id"]=='"$SA"'] == ["Seq A by student1"]' "student2 does not see student1's edit"
check '[s["holder"] for s in d["result"]["sequences"] if s["id"]=='"$SA"'] == ["student1"]' "student2 does not see student1's lock"
same_split "after concurrent edits"

STEP="imports"
say "both import media at the same moment"
echo "import $WORK/from1.$EXT" >&3
echo "import $WORK/from2.$EXT" >&5
IFS= read -r -t 180 R1 <&4 || die "student1 gave no answer to import"
IFS= read -r -t 180 R2 <&6 || die "student2 gave no answer to import"
[ "$(jget "$R1" 'd["ok"]')" = True ] || die "student1's import failed: $R1"
[ "$(jget "$R2" 'd["ok"]')" = True ] || die "student2's import failed: $R2"
sync_both
for n in 1 2; do
  ok "$n" status
  check '{"from1.'"$EXT"'","from2.'"$EXT"'"} <= {m["name"] for m in d["result"]["media"] if m["in_bin"]}' \
    "student$n's bins lack an import: $(result '[m["name"] for m in d["result"]["media"]]')"
  check 'd["result"]["upload_failures"] == []' "student$n has upload failures: $(result 'd["result"]["upload_failures"]')"
done
[ -f "$WORK/drive/drive/Team Projects/E2E Media/from1.$EXT" ] || die "from1 is not in the project's media folder on the Drive"
[ -f "$WORK/drive/drive/Team Projects/E2E Media/from2.$EXT" ] || die "from2 is not in the project's media folder on the Drive"
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

STEP="quit"
echo quit >&3
echo quit >&5
wait "$M1_PID" || die "student1's session exited with an error"
wait "$M2_PID" || die "student2's session exited with an error"
M1_PID=""
M2_PID=""
say "PASSED: team editing end to end against a local Drive"
