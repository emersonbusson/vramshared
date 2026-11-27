#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/package/render-release-notes.sh
#
# Render standardized vRamShared GitHub Release notes for SemVer v1.0.x+ tags.
# Prefers a curated section from CHANGELOG.md when present and falls back to
# structured Conventional-Commit grouping from GitHub generated release notes.
set -euo pipefail

TAG=""
REPO=""
OUT=""
CHANGELOG=""
PREV_TAG=""
GENERATED_NOTES_FILE=""
GIT_DIR_ARG=""

usage() {
  cat >&2 <<'EOF'
Usage: render-release-notes.sh --tag <vX.Y.Z> --repo <owner/repo> --out <file>
  [--changelog <path>] [--previous-tag <vA.B.C>]
  [--generated-notes-file <path>] [--git-dir <repo-root>]
EOF
  exit 64
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --tag)
      TAG="${2:-}"
      shift 2
      ;;
    --repo)
      REPO="${2:-}"
      shift 2
      ;;
    --out)
      OUT="${2:-}"
      shift 2
      ;;
    --changelog)
      CHANGELOG="${2:-}"
      shift 2
      ;;
    --previous-tag)
      PREV_TAG="${2:-}"
      shift 2
      ;;
    --generated-notes-file)
      GENERATED_NOTES_FILE="${2:-}"
      shift 2
      ;;
    --git-dir)
      GIT_DIR_ARG="${2:-}"
      shift 2
      ;;
    *)
      usage
      ;;
  esac
done

if [[ -z "$TAG" || -z "$REPO" || -z "$OUT" ]]; then
  usage
fi

if [[ ! "$TAG" =~ ^v[1-9][0-9]*\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
  printf 'refused: tag is outside the public v1.x SemVer format: %s\n' "$TAG" >&2
  exit 2
fi

if [[ ! "$REPO" =~ ^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$ ]]; then
  printf 'refused: repository must be owner/name: %s\n' "$REPO" >&2
  exit 2
fi

if [[ -n "$PREV_TAG" && ! "$PREV_TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  printf 'refused: invalid previous tag: %s\n' "$PREV_TAG" >&2
  exit 2
fi

if [[ -z "$PREV_TAG" && -n "$GIT_DIR_ARG" ]]; then
  PREV_TAG=$(git -C "$GIT_DIR_ARG" tag --list 'v*' --sort=-v:refname | awk -v cur="$TAG" '$0 != cur { print; exit }')
fi

tmp_generated=""
cleanup() {
  if [[ -n "$tmp_generated" && -f "$tmp_generated" ]]; then
    rm -f -- "$tmp_generated"
  fi
}
trap cleanup EXIT

PYTHONDONTWRITEBYTECODE=1 python3 - "$TAG" "$REPO" "$OUT" "$CHANGELOG" "$PREV_TAG" "$GENERATED_NOTES_FILE" <<'PY'
import json
import os
import re
import subprocess
import sys

tag, repo, out_path, changelog_path, prev_tag, gen_file = sys.argv[1:7]
version = tag[1:]

def extract_changelog_section(path: str, ver: str) -> str:
    if not path or not os.path.isfile(path):
        return ""
    with open(path, "r", encoding="utf-8") as fh:
        lines = fh.readlines()
    header_re = re.compile(r"^##\s+\[" + re.escape(ver) + r"\](?:\s|$)")
    next_re = re.compile(r"^##\s+")
    collecting = False
    collected = []
    for line in lines:
        if not collecting:
            if header_re.match(line):
                collecting = True
            continue
        if next_re.match(line):
            break
        collected.append(line.rstrip("\n"))
    # Trim leading/trailing blank lines
    while collected and not collected[0].strip():
        collected.pop(0)
    while collected and not collected[-1].strip():
        collected.pop()
    # Normalize ### headings to ## subsections inside release notes
    normalized = []
    for line in collected:
        if line.startswith("### "):
            normalized.append("## " + line[4:])
        else:
            normalized.append(line)
    return "\n".join(normalized).strip()

def load_generated_notes() -> str:
    if gen_file:
        if not os.path.isfile(gen_file):
            raise SystemExit(f"refused: generated notes file not found: {gen_file}")
        with open(gen_file, "r", encoding="utf-8") as fh:
            raw = fh.read().strip()
        if raw.startswith("{"):
            data = json.loads(raw)
            return str(data.get("body", "")).strip()
        return raw
    cmd = [
        "gh", "api", f"repos/{repo}/releases/generate-notes",
        "-f", f"tag_name={tag}",
    ]
    if prev_tag:
        cmd.extend(["-f", f"previous_tag_name={prev_tag}"])
    res = subprocess.run(cmd, capture_output=True, text=True, check=True)
    data = json.loads(res.stdout)
    return str(data.get("body", "")).strip()

def format_fallback_body(raw_body: str, fallback_prev: str) -> tuple[str, str]:
    compare_prev = fallback_prev
    compare_match = re.search(
        r"https://github\.com/[^/\s]+/[^/\s]+/compare/(v[0-9]+\.[0-9]+\.[0-9]+)\.\.\.",
        raw_body,
    )
    if compare_match and not compare_prev:
        compare_prev = compare_match.group(1)

    features = []
    fixes = []
    security_pkg = []
    other = []
    release_chores = []

    bullet_re = re.compile(r"^\*\s+(.*)$")
    pr_tail_re = re.compile(
        r"\s+by\s+@[^\s]+\s+in\s+(https://github\.com/[^/\s]+/[^/\s]+/pull/([0-9]+))\s*$"
    )
    pr_only_re = re.compile(
        r"\s+in\s+(https://github\.com/[^/\s]+/[^/\s]+/pull/([0-9]+))\s*$"
    )
    cc_re = re.compile(r"^([a-z]+)(?:\(([^)]+)\))?(!)?:\s*(.+)$")

    for line in raw_body.splitlines():
        m = bullet_re.match(line.strip())
        if not m:
            continue
        item = m.group(1).strip()
        pr_suffix = ""
        pm = pr_tail_re.search(item) or pr_only_re.search(item)
        if pm:
            pr_url, pr_num = pm.group(1), pm.group(2)
            item = item[:pm.start()].strip()
            pr_suffix = f" ([#{pr_num}]({pr_url}))"

        cm = cc_re.match(item)
        if cm:
            kind, scope, _bang, desc = cm.group(1), cm.group(2) or "", cm.group(3), cm.group(4).strip()
            desc = desc[0].upper() + desc[1:] if desc else desc
            scope_prefix = f"**{scope}:** " if scope else ""
            formatted = f"- {scope_prefix}{desc}{pr_suffix}"
            if kind == "chore" and scope == "release":
                release_chores.append(formatted)
            elif kind == "feat":
                features.append(formatted)
            elif kind == "fix":
                fixes.append(formatted)
            elif kind in ("security", "ci", "build") or scope in ("release", "package", "packaging", "security"):
                security_pkg.append(formatted)
            else:
                other.append(formatted)
        else:
            if item:
                other.append(f"- {item}{pr_suffix}")

    sections = []
    if features:
        sections.append("### Features\n\n" + "\n".join(features))
    if fixes:
        sections.append("### Fixes\n\n" + "\n".join(fixes))
    if security_pkg:
        sections.append("### Security, CI & Packaging\n\n" + "\n".join(security_pkg))
    if other:
        sections.append("### Other Changes\n\n" + "\n".join(other))
    if not sections and release_chores:
        sections.append("### Release\n\n" + "\n".join(release_chores))

    if not sections:
        raise SystemExit("refused: release notes contain no highlights")

    return "## Highlights\n\n" + "\n\n".join(sections), compare_prev

curated = extract_changelog_section(changelog_path, version)
compare_prev = prev_tag
if curated:
    highlights_block = curated
else:
    raw = load_generated_notes()
    if not raw:
        raise SystemExit("refused: empty release notes from both CHANGELOG.md and GitHub API")
    highlights_block, compare_prev = format_fallback_body(raw, prev_tag)

parts = [
    f"# vRamShared {tag}",
    "",
    f"Validated release on the `v1.0.x` line (`{tag}`).",
    "",
    highlights_block,
    "",
    "## Install / Upgrade",
    "",
    "```bash",
    f"curl -fsSL https://raw.githubusercontent.com/{repo}/{tag}/scripts/install.sh | sudo bash",
    "```",
    "",
    "## Verification",
    "",
    "Verify package integrity against `SHA256SUMS.txt` before installing. All Linux packages retain binary and systemd compatibility (`ramshared` / `ramsharedd`).",
]

if compare_prev:
    parts.extend([
        "",
        f"**Full Changelog**: https://github.com/{repo}/compare/{compare_prev}...{tag}",
    ])

output_text = "\n".join(parts).rstrip() + "\n"
os.makedirs(os.path.dirname(os.path.abspath(out_path)), exist_ok=True)
with open(out_path, "w", encoding="utf-8") as fh:
    fh.write(output_text)
PY
