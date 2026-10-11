"""Checks run against https://llm.imabee.com by run.sh: an account, an API key from the console, the
published client completing inference with that key, and the key refused once revoked.

Prints no key, password or cookie: CI logs are public.
"""

import http.cookiejar
import json
import os
import re
import secrets
import subprocess
import sys
import urllib.error
import urllib.parse
import urllib.request

BASE = "https://llm.imabee.com"
CORTEX, WORK = sys.argv[1], sys.argv[2]
web = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar()))


def post(path, fields):
    with web.open(BASE + path, urllib.parse.urlencode(fields).encode(), timeout=60) as r:
        return r.read().decode()


def console():
    with web.open(BASE + "/account/api-keys", timeout=60) as r:
        return r.read().decode()


def csrf(page):
    return re.search(r'name="csrf" value="([0-9a-f]{64})"', page).group(1)


def probe(key):
    req = urllib.request.Request(BASE + "/v1/api-key", headers={"Authorization": "Bearer " + key})
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return r.status
    except urllib.error.HTTPError as e:
        return e.code


def cortex(key, name):
    """One headless turn in a fresh home; returns the exit code and the parsed result, if any."""
    home = os.path.join(WORK, name)
    os.makedirs(home)
    env = {k: v for k, v in os.environ.items() if not k.startswith("CORTEX_")}
    env.update(HOME=home, CORTEX_HOME=os.path.join(home, ".cortex"), CORTEX_API_KEY=key, CORTEX_DISABLE_AUTOUPDATER="1")
    p = subprocess.run(
        [CORTEX, "-p", "Reply with the single word pong.", "--output-format", "json"],
        env=env, cwd=home, capture_output=True, text=True, timeout=900,
    )
    out = (p.stdout + p.stderr).replace(key, "<key>")
    with open(os.path.join(WORK, name + ".log"), "w") as f:
        f.write(out)
    try:
        result = json.loads(p.stdout)
    except ValueError:
        result = None
    return p.returncode, result, out


def check(ok, message):
    print(("ok   " if ok else "FAIL ") + message, flush=True)
    if not ok:
        sys.exit(1)


post("/register", {
    "email": "e2e@example.com", "password": secrets.token_urlsafe(24),
    "first_name": "End", "last_name": "ToEnd", "next": "/account/api-keys",
})
page = post("/account/api-keys", {"csrf": csrf(console()), "name": "e2e"})
shown = re.search(r'<code class="secret">([^<]+)</code>', page)
check(shown is not None, "console created a key and showed it once")
key = shown.group(1)
check(probe(key) == 200, "the key passes the client's /v1/api-key probe")

rc, result, out = cortex(key, "with-key")
if rc != 0:
    print(out[-2000:])
check(
    rc == 0 and isinstance(result, dict) and result.get("type") != "error" and result.get("stopReason"),
    "cortex -p completed a turn with the key",
)
check((result.get("usage") or {}).get("output_tokens", 0) > 0, "the model generated output tokens")

key_id = re.search(r'name="id" value="([0-9a-f-]{36})"', console()).group(1)
check("Key revoked" in post("/account/api-keys/revoke", {"csrf": csrf(console()), "id": key_id}), "the console revoked the key")
check(probe(key) == 401, "a revoked key fails the probe")
rc, _, _ = cortex(key, "revoked")
check(rc != 0, "cortex -p refuses to run with a revoked key")
