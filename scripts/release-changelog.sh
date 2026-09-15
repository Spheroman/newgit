#!/usr/bin/env bash
# Fold changelog.d/ fragments into CHANGELOG.md under a new version heading.
#
#   scripts/release-changelog.sh 0.3.0 [YYYY-MM-DD]   fold and delete fragments
#   scripts/release-changelog.sh --check              validate fragment names
#
# One file per change means two branches never touch the same lines, so the
# changelog stops being a queue every parallel branch waits in (#36).
set -euo pipefail

cd "$(dirname "$0")/.."

FRAGMENT_DIR="changelog.d"
CHANGELOG="CHANGELOG.md"

# Keep-a-changelog's order, plus the `Documentation` section this file already
# uses. A fragment's category is its filename prefix: `fixed-28-status.md`.
CATEGORIES=(added changed deprecated removed fixed security documentation)

title_of() {
  case "$1" in
    added) echo Added ;;
    changed) echo Changed ;;
    deprecated) echo Deprecated ;;
    removed) echo Removed ;;
    fixed) echo Fixed ;;
    security) echo Security ;;
    documentation) echo Documentation ;;
  esac
}

# Every fragment must be reachable by the category globs below. A typo'd
# prefix would otherwise be skipped in silence and the entry would vanish
# from the release, which is exactly the failure the release workflow's
# empty-notes check exists to prevent.
check_fragments() {
  local bad=0 f base prefix known
  shopt -s nullglob
  for f in "$FRAGMENT_DIR"/*.md; do
    base="$(basename "$f")"
    [ "$base" = "README.md" ] && continue
    prefix="${base%.md}"
    prefix="${prefix%%-*}"
    known=0
    for c in "${CATEGORIES[@]}"; do
      [ "$prefix" = "$c" ] && known=1
    done
    if [ "$known" -eq 0 ]; then
      echo "$f: filename must start with one of: ${CATEGORIES[*]}" >&2
      bad=1
      continue
    fi
    if [ "$base" = "$prefix.md" ]; then
      echo "$f: name the change too, e.g. $prefix-28-status-truth.md" >&2
      bad=1
    fi
    # Fragments are spliced in as list items under a `### Category` heading,
    # so one that is not a list starts a section that renders wrong.
    if ! grep -qE '^- ' "$f" || [ -n "$(sed -n '/./{p;q;}' "$f" | grep -v '^- ' || true)" ]; then
      echo "$f: must start with a '- ' list item" >&2
      bad=1
    fi
  done
  shopt -u nullglob
  return "$bad"
}

if [ "${1:-}" = "--check" ]; then
  check_fragments
  echo "changelog fragments OK"
  exit 0
fi

version="${1:-}"
date="${2:-$(date +%F)}"
if [ -z "$version" ]; then
  sed -n '2,7p' "$0" >&2
  exit 2
fi

check_fragments

shopt -s nullglob
all=("$FRAGMENT_DIR"/*-*.md)
shopt -u nullglob
if [ "${#all[@]}" -eq 0 ]; then
  echo "no fragments in $FRAGMENT_DIR/; nothing to release" >&2
  exit 1
fi

if grep -qE "^## \[?$version" "$CHANGELOG"; then
  echo "$CHANGELOG already has a section for $version" >&2
  exit 1
fi

prev="$(sed -n 's/^## \[\([0-9][^]]*\)\].*/\1/p' "$CHANGELOG" | head -1)"
if [ -z "$prev" ]; then
  echo "could not find the previous version heading in $CHANGELOG" >&2
  exit 1
fi

section="$(mktemp)"
trap 'rm -f "$section" "$section.new"' EXIT

{
  printf '## [%s] — %s\n' "$version" "$date"
  for c in "${CATEGORIES[@]}"; do
    shopt -s nullglob
    files=("$FRAGMENT_DIR/$c"-*.md)
    shopt -u nullglob
    [ "${#files[@]}" -eq 0 ] && continue
    printf '\n### %s\n\n' "$(title_of "$c")"
    for f in "${files[@]}"; do
      # Trailing blank lines are the fragment file's business; the separation
      # between entries is this script's.
      awk 'BEGIN{blank=0} /^[[:space:]]*$/{blank++; next} {while(blank--)print ""; blank=0; print}' "$f"
      printf '\n'
    done
  done
} > "$section"

awk -v secfile="$section" '
  /^## \[[0-9]/ && !spliced {
    while ((getline line < secfile) > 0) print line
    spliced = 1
  }
  { print }
' "$CHANGELOG" > "$section.new"

# Keep the compare links honest: Unreleased now starts at the new tag, and
# the new version compares against the one it followed.
awk -v prev="$prev" -v ver="$version" '
  /^\[Unreleased\]: / {
    base = $2
    sub(/compare\/v.*$/, "", base)
    print "[Unreleased]: " base "compare/v" ver "...HEAD"
    print "[" ver "]: " base "compare/v" prev "...v" ver
    next
  }
  { print }
' "$section.new" > "$CHANGELOG"

rm -f "${all[@]}"

echo "folded ${#all[@]} fragment(s) into $CHANGELOG as $version ($date)"
echo "next: bump Cargo.toml, commit, tag v$version"
