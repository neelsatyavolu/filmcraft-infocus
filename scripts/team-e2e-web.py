#!/usr/bin/env python3
"""Browser-side helper for scripts/team-e2e.sh: what the editing dashboard and other HTTP clients
do against the dev Drive (scripts/dev/team_e2e_server.py in infocus-drive). Standard library only.

  team-e2e-web.py upload <url> <user> <project id> <file> [<folder in the media folder>]
      The dashboard's footage upload: a cookie session from the dev server's /dev/sign-in, the
      team upload session, 8 MiB chunks four at a time through /api/upload/chunk, then complete.
      Prints {"bytes", "secs", "mb_s"}.
  team-e2e-web.py get <url> <user | @token file> <path>
      One GET as a browser session (user name) or a CLI token (@file). Prints {"status", "body"}.
"""
import concurrent.futures
import http.cookiejar
import json
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

CHUNK = 8 * 1024 * 1024
STREAMS = 4


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


def browser(url, user):
    """An opener holding the dev server's session cookie for `user`."""
    jar = http.cookiejar.CookieJar()
    opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(jar), NoRedirect)
    q = urllib.parse.urlencode({"user": user, "next": "/editing"})
    try:
        opener.open(f"{url}/dev/sign-in?{q}", timeout=30)
    except urllib.error.HTTPError as e:
        if e.code != 303:
            raise SystemExit(f"dev sign-in as {user} failed: HTTP {e.code}")
    if not any(True for _ in jar):
        raise SystemExit(f"dev sign-in as {user} set no cookie")
    return opener, {}


def client(url, who):
    if who.startswith("@"):
        with open(who[1:], encoding="utf-8") as f:
            token = f.read().strip()
        return urllib.request.build_opener(NoRedirect), {"Authorization": f"Bearer {token}"}
    return browser(url, who)


def call(opener, headers, method, url, body=None, raw=None):
    data, h = None, dict(headers, Accept="application/json")
    if body is not None:
        data, h["Content-Type"] = json.dumps(body).encode(), "application/json"
    elif raw is not None:
        data, h["Content-Type"] = raw, "application/octet-stream"
    req = urllib.request.Request(url, data=data, method=method, headers=h)
    try:
        with opener.open(req, timeout=120) as r:
            status, text = r.status, r.read()
    except urllib.error.HTTPError as e:
        status, text = e.code, e.read()
    try:
        parsed = json.loads(text.decode() or "null")
    except (UnicodeDecodeError, json.JSONDecodeError):
        parsed = None
    return status, parsed


def upload(url, user, pid, path, folder=""):
    opener, h = browser(url, user)
    size = os.path.getsize(path)
    team = f"{url}/api/team/projects/{urllib.parse.quote(pid, safe='')}"
    start = time.monotonic()
    status, init = call(opener, h, "POST", f"{team}/media/upload/init",
                        {"name": os.path.basename(path), "size": size, "chunk_size": CHUNK, "path": folder})
    if status != 200:
        raise SystemExit(f"upload init: HTTP {status} {init}")
    uid, chunk = init["upload_id"], int(init.get("chunk_size") or CHUNK)
    count = max(1, -(-size // chunk))

    def put(i):
        with open(path, "rb") as f:
            f.seek(i * chunk)
            data = f.read(chunk)
        q = urllib.parse.urlencode({"upload_id": uid, "index": i})
        st, body = call(opener, h, "PUT", f"{url}/api/upload/chunk?{q}", raw=data)
        if st != 200:
            raise SystemExit(f"chunk {i}: HTTP {st} {body}")

    with concurrent.futures.ThreadPoolExecutor(STREAMS) as pool:
        list(pool.map(put, range(count)))
    status, done = call(opener, h, "POST", f"{team}/media/upload/complete", {"upload_id": uid})
    if status != 200:
        raise SystemExit(f"upload complete: HTTP {status} {done}")
    secs = time.monotonic() - start
    print(json.dumps({"bytes": size, "secs": round(secs, 3), "mb_s": round(size / 1e6 / max(secs, 1e-6), 1)}))


def get(url, who, path):
    opener, h = client(url, who)
    status, body = call(opener, h, "GET", f"{url}{path}")
    print(json.dumps({"status": status, "body": body}))


def main(argv):
    if len(argv) >= 5 and argv[1] == "upload":
        upload(*argv[2:7])
    elif len(argv) == 5 and argv[1] == "get":
        get(*argv[2:5])
    else:
        raise SystemExit(__doc__)


if __name__ == "__main__":
    main(sys.argv)
