#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/package/test-render-release-notes.sh
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
SCRIPT="$ROOT/scripts/package/render-release-notes.sh"
TMP=$(mktemp -d)
trap 'rm -rf -- "$TMP"' EXIT

# 1. Legitimate curated CHANGELOG.md path
cat > "$TMP/CHANGELOG.md" <<'EOF'
# Changelog

## [1.0.6] - 2026-11-28

### Fixed

- Standardize automated release notes for every v1.0.x publication.

## [1.0.5] - 2026-11-27

### Added

- Previous entry.
EOF

bash "$SCRIPT" \
  --tag v1.0.6 \
  --repo emersonbusson/vramshared \
  --changelog "$TMP/CHANGELOG.md" \
  --previous-tag v1.0.5 \
  --out "$TMP/curated.md"

grep -Fq "# vRamShared v1.0.6" "$TMP/curated.md"
grep -Fq "## Fixed" "$TMP/curated.md"
grep -Fq "Standardize automated release notes" "$TMP/curated.md"
grep -Fq "https://raw.githubusercontent.com/emersonbusson/vramshared/v1.0.6/scripts/install.sh" "$TMP/curated.md"
grep -Fq "SHA256SUMS.txt" "$TMP/curated.md"
grep -Fq "https://github.com/emersonbusson/vramshared/compare/v1.0.5...v1.0.6" "$TMP/curated.md"
! grep -Fq "Previous entry." "$TMP/curated.md"
echo "PASS render-release-notes uses curated CHANGELOG.md section"

# 2. Legitimate fallback path from GitHub generated-notes payload
cat > "$TMP/generated.json" <<'EOF'
{
  "name": "v1.0.7",
  "body": "## What's Changed\n* feat(pooled-cache): add batched page readback by @emersonbusson in https://github.com/emersonbusson/vramshared/pull/40\n* fix(installer): verify staged digest before pivot by @emersonbusson in https://github.com/emersonbusson/vramshared/pull/41\n* ci: verify release-notes rendering by @emersonbusson in https://github.com/emersonbusson/vramshared/pull/42\n* chore(release): prepare RamShared 1.0.7 by @emersonbusson in https://github.com/emersonbusson/vramshared/pull/43\n\n**Full Changelog**: https://github.com/emersonbusson/vramshared/compare/v1.0.6...v1.0.7"
}
EOF

bash "$SCRIPT" \
  --tag v1.0.7 \
  --repo emersonbusson/vramshared \
  --changelog "$TMP/CHANGELOG.md" \
  --generated-notes-file "$TMP/generated.json" \
  --out "$TMP/fallback.md"

grep -Fq "# vRamShared v1.0.7" "$TMP/fallback.md"
grep -Fq "## Highlights" "$TMP/fallback.md"
grep -Fq "### Features" "$TMP/fallback.md"
grep -Fq "### Fixes" "$TMP/fallback.md"
grep -Fq "### Security, CI & Packaging" "$TMP/fallback.md"
grep -Fq "[#40](https://github.com/emersonbusson/vramshared/pull/40)" "$TMP/fallback.md"
grep -Fq "[#41](https://github.com/emersonbusson/vramshared/pull/41)" "$TMP/fallback.md"
! grep -Fq "by @emersonbusson" "$TMP/fallback.md"
! grep -Fq "prepare RamShared 1.0.7" "$TMP/fallback.md"
grep -Fq "https://github.com/emersonbusson/vramshared/compare/v1.0.6...v1.0.7" "$TMP/fallback.md"
echo "PASS render-release-notes formats fallback generated notes cleanly"

# 3. Refusal tests (#13): invalid tags, invalid repo, empty generated notes
for bad_tag in "v1.0.6-dirty" "1.0.6" "v0.14.1" "../v1.0.6"; do
  if bash "$SCRIPT" --tag "$bad_tag" --repo emersonbusson/vramshared --generated-notes-file "$TMP/generated.json" --out "$TMP/bad.md" 2>/dev/null; then
    echo "FAIL expected refusal for bad tag: $bad_tag" >&2
    exit 1
  fi
done
echo "PASS render-release-notes refuses invalid SemVer tags"

if bash "$SCRIPT" --tag v1.0.7 --repo "bad repo" --generated-notes-file "$TMP/generated.json" --out "$TMP/bad.md" 2>/dev/null; then
  echo "FAIL expected refusal for bad repo" >&2
  exit 1
fi
echo "PASS render-release-notes refuses invalid repository identifier"

printf '{"body": ""}\n' > "$TMP/empty.json"
if bash "$SCRIPT" --tag v1.0.8 --repo emersonbusson/vramshared --changelog "$TMP/CHANGELOG.md" --generated-notes-file "$TMP/empty.json" --out "$TMP/bad.md" 2>/dev/null; then
  echo "FAIL expected refusal for empty generated notes" >&2
  exit 1
fi
echo "PASS render-release-notes refuses empty release notes"
