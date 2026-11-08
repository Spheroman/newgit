#!/usr/bin/env bash
# Deploy the working tree to the dev remote (wrangler.jsonc's `env.dev`):
# its own Worker on workers.dev, with its own Artifacts namespace, Durable
# Objects, and container, sharing nothing with newgit.dev. Try a change
# there before it merges, rather than merging to find out on prod.
#
#   remote/scripts/dev.sh           the Worker from this tree, and prod's newest
#                                   container image (most changes are Worker-only)
#   remote/scripts/dev.sh --build   the container built from this tree too (needs Docker)
#
# Its secrets are its own, set once: TOKEN_KEY and ALLOWED_LOGINS
# (`npx wrangler secret put <name> --env dev`).
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ "${1:-}" == "--build" ]]; then
	exec npx wrangler deploy --env dev
fi

account=$(node -e 'const t=require("fs").readFileSync("wrangler.jsonc","utf8");console.log(t.match(/"CLOUDFLARE_ACCOUNT_ID": "([0-9a-f]+)"/)[1])')
tag=$(npx wrangler containers images list 2>/dev/null |
	awk '$1 == "newgit-gitendpoint-endpoint" && $2 ~ /^wrangler-/ { print $2 }' | sort | tail -1)
if [[ -z "$tag" ]]; then
	echo "no prod endpoint image to reuse: run with --build where Docker is" >&2
	exit 1
fi
# Wrangler takes a managed image only pinned by digest: ask the registry.
credentials=$(npx wrangler containers registries credentials registry.cloudflare.com --pull --json 2>/dev/null)
field() { node -e 'let d="";process.stdin.on("data",c=>d+=c).on("end",()=>console.log(JSON.parse(d)[process.argv[1]]))' "$1"; }
digest=$(curl -fsSI -u "$(field username <<<"$credentials"):$(field password <<<"$credentials")" \
	-H 'Accept: application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json, application/vnd.docker.distribution.manifest.list.v2+json' \
	"https://registry.cloudflare.com/v2/$account/newgit-gitendpoint-endpoint/manifests/$tag" |
	tr -d '\r' | awk 'tolower($1) == "docker-content-digest:" { print $2 }')
if [[ -z "$digest" ]]; then
	echo "the registry gave no digest for $tag" >&2
	exit 1
fi
image="registry.cloudflare.com/$account/newgit-gitendpoint-endpoint@$digest"
config=.wrangler.dev.jsonc
trap 'rm -f "$config"' EXIT
node - "$image" > "$config" <<'JS'
// The dev environment's container, from `image` instead of the Dockerfile.
const fs = require("fs");
const text = fs.readFileSync("wrangler.jsonc", "utf8");
const at = text.indexOf('"env"');
const tail = text.slice(at).replace(
	/"endpoint": \{ "dockerfile": "\.\/endpoint\.Dockerfile", "build_context": "\.\." \}/,
	`"endpoint": { "image": "${process.argv[2]}" }`,
);
process.stdout.write(text.slice(0, at) + tail);
JS
echo "dev remote: container image $tag ($digest)"
npx wrangler deploy --env dev --config "$config"
