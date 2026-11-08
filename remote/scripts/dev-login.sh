#!/usr/bin/env bash
# Log in to the dev remote without a browser: a login token from its dev-only
# route (`POST /v1/dev/login`, with its DEV_LOGIN_SECRET), saved where
# `newgit login` saves one, for that remote's URL only. Other remotes'
# logins are left as they are.
#
#   remote/scripts/dev-login.sh [login]     (default: spheroman)
#
# The secrets are in ~/.newgit/dev-remote.env, made when the dev remote was.
set -euo pipefail
url=${NEWGIT_DEV_URL:-https://newgit-dev.jackwen04.workers.dev}
login=${1:-spheroman}
secret=$(grep '^DEV_LOGIN_SECRET=' "$HOME/.newgit/dev-remote.env" | cut -d= -f2-)
answer=$(curl -fsS -X POST "$url/v1/dev/login" -H "x-dev-login: $secret" -H 'content-type: application/json' \
	-H 'user-agent: newgit-dev-login' -d "{\"login\":\"$login\"}")
python3 -I - "$HOME/.newgit/auth.toml" "$url" "$answer" <<'PY'
import json, re, sys
path, url, answer = sys.argv[1], sys.argv[2], json.loads(sys.argv[3])
try:
    text = open(path).read()
except FileNotFoundError:
    text = ""
section = f'[remotes."{url}"]'
entry = f'{section}\nlogin = "{answer["login"]}"\ntoken = "{answer["token"]}"\ntoken_id = "{answer["token_id"]}"\n'
# This remote's table, up to the next table, replaced; or added.
pattern = re.compile(re.escape(section) + r"\n(?:(?!\[).*\n?)*")
text = pattern.sub(entry, text) if section in text else (text.rstrip("\n") + "\n\n" + entry if text else entry)
open(path, "w").write(text)
print(f"logged in to {url} as {answer['login']}")
PY
