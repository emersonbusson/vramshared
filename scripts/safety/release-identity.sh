#!/usr/bin/env bash
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/release-identity.sh
# release-identity.sh — the single release-identity ordering predicate.
#
# §RF-7/DT-6. Both release entry points (`scripts/safety/auto-update.sh` and
# `scripts/install-release.sh`) source this so there is one ordering rule and
# it is never re-derived (RF-8).
#
# `git describe` output is `vMAJOR.MINOR.PATCH-COUNT-gHASH[-dirty]`. The
# `COUNT` is the commit count since the tag and is monotonic **for one tag
# base**; the full string is not a version and lexical order over it is
# meaningless. So a candidate is strictly newer only when it shares the tag
# base and its COUNT is greater. Anything that cannot be compared (a different
# base, a dirty candidate, an unparseable identity) is refused — a downgrade
# must never install on a guess.
#
# `release_is_newer <candidate> <running>` prints one of `newer`, `older`,
# `equal`, `unknown`. Callers must inspect the verdict, not the exit status.

release_cmp_uint() {
  local a="$1" b="$2"
  if (( ${#a} > ${#b} )) || { (( ${#a} == ${#b} )) && [[ $a > $b ]]; }; then
    printf 'gt\n'
  elif [[ $a != "$b" ]]; then
    printf 'lt\n'
  else
    printf 'eq\n'
  fi
}

release_is_newer() {
  local candidate="$1" running="$2"
  case "$candidate" in
    *-dirty|*+dirty*) printf 'unknown\n'; return 0 ;;
  esac
  local cand_parts run_parts LC_ALL=C
  cand_parts=$(release_identity_parts "$candidate") || { printf 'unknown\n'; return 0; }
  run_parts=$(release_identity_parts "$running") || { printf 'unknown\n'; return 0; }
  local c_maj c_min c_pat c_cnt r_maj r_min r_pat r_cnt cmp
  read -r c_maj c_min c_pat c_cnt <<<"$cand_parts"
  read -r r_maj r_min r_pat r_cnt <<<"$run_parts"
  for pair in "$c_maj:$r_maj" "$c_min:$r_min" "$c_pat:$r_pat" "$c_cnt:$r_cnt"; do
    cmp=$(release_cmp_uint "${pair%%:*}" "${pair#*:}")
    if [[ $cmp == gt ]]; then
      printf 'newer\n'
      return 0
    elif [[ $cmp == lt ]]; then
      printf 'older\n'
      return 0
    fi
  done
  printf 'equal\n'
  return 0
}

# Accept bounded SemVer release identities (`vMAJOR.MINOR.PATCH` or
# `vMAJOR.MINOR.PATCH-COUNT-gHASH`), with dirty marks allowed only for the
# running identity. Leading zeros on numeric segments are refused.
release_identity_parts() {
  local value=$1
  local exact='^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-dirty|\+dirty)?$'
  local described='^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)-(0|[1-9][0-9]*)-g[[:xdigit:]]{4,64}(-dirty|\+dirty)?$'
  [[ ${#value} -le 128 ]] || return 1
  if [[ $value =~ $described ]]; then
    printf '%s %s %s %s\n' "${BASH_REMATCH[1]}" "${BASH_REMATCH[2]}" "${BASH_REMATCH[3]}" "${BASH_REMATCH[4]}"
  elif [[ $value =~ $exact ]]; then
    printf '%s %s %s 0\n' "${BASH_REMATCH[1]}" "${BASH_REMATCH[2]}" "${BASH_REMATCH[3]}"
  else
    return 1
  fi
}

# Select an existing bundle without splitting pathnames or following package
# directory symlinks. Use a stable path tie-break when timestamps are equal.
release_newest_bundle() (
  local PATH=/usr/sbin:/usr/bin:/sbin:/bin LC_ALL=C TZ=UTC
  export PATH LC_ALL TZ
  local root=$1 candidate timestamp newest='' newest_time='' scan_fd scan_pid
  exec {scan_fd}< <(find "$root" -mindepth 2 -maxdepth 2 -type d -name release -print0 2>/dev/null) || return 1
  scan_pid=$!
  while IFS= read -r -d '' candidate <&"$scan_fd"; do
    if ! timestamp=$(stat -c '%y' -- "$candidate"); then
      exec {scan_fd}<&-
      wait "$scan_pid" || :
      return 1
    fi
    if [[ -z $newest || $timestamp > $newest_time ||
      ( $timestamp == "$newest_time" && $candidate < $newest ) ]]; then
      newest=$candidate
      newest_time=$timestamp
    fi
  done
  exec {scan_fd}<&-
  wait "$scan_pid" || return 1
  printf '%s\n' "$newest"
)

# Bounded release manifests and detached signatures are metadata, not an
# unbounded input stream.
release_manifest_max_bytes() {
  printf '1048576\n'
}

# A trust object is valid only when the file and every directory above it are
# root-owned and not writable by group/other. A root-owned sticky directory
# such as /tmp is safe as an ancestor because another user cannot replace the
# root-owned child entry. Symlink components and non-normalized paths refuse.
release_path_is_root_controlled() {
  # Trust checks can run from a root service with an inherited caller PATH.
  # Resolve the metadata reader only from standard system command directories.
  local PATH=/usr/sbin:/usr/bin:/sbin:/bin
  local path=$1 current=$1 owner mode metadata
  [[ $path == /* && $path != *$'\n'* && $path != *'//'* &&
    $path != *'/./'* && $path != */. && $path != *'/../'* && $path != */.. ]] || return 1
  while :; do
    if [[ $current == "$path" ]]; then
      { [[ -f $current ]] || [[ -d $current ]]; } && [[ ! -L $current ]] || return 1
    else
      [[ -d $current && ! -L $current ]] || return 1
    fi
    metadata=$(command stat -c '%u %a' -- "$current") || return 1
    read -r owner mode <<<"$metadata"
    [[ $owner == 0 && $mode =~ ^[0-7]{3,4}$ ]] || return 1
    if (( (8#$mode & 8#022) != 0 )); then
      [[ $current != "$path" && -d $current ]] &&
        (( (8#$mode & 8#1000) != 0 )) || return 1
    fi
    [[ $current == / ]] && break
    current=${current%/*}
    [[ -n $current ]] || current=/
  done
}

# Verify the release manifest with a trust keyring provisioned independently
# of the bundle. SHA256SUMS alone is integrity metadata, not authorization.
# The keyring and its containing directory must be root-owned and not writable
# by group/other so an unattended root updater cannot trust a caller-edited
# key. A human-supplied digest for the same manifest is the separate attended
# path handled by install-release.sh.
release_verify_signed_manifest() {
  local PATH=/usr/sbin:/usr/bin:/sbin:/bin
  local bundle=$1 keyring=$2 manifest signature manifest_bytes signature_bytes max_bytes verifier
  [[ $bundle == /* && -d $bundle && ! -L $bundle ]] || return 1
  [[ $keyring == /* && -f $keyring && ! -L $keyring ]] || return 1
  release_path_is_root_controlled "$bundle" || return 1
  release_path_is_root_controlled "$keyring" || return 1
  manifest="$bundle/SHA256SUMS"
  signature="$manifest.asc"
  [[ -f $manifest && ! -L $manifest && -f $signature && ! -L $signature ]] || return 1
  release_path_is_root_controlled "$manifest" || return 1
  release_path_is_root_controlled "$signature" || return 1
  max_bytes=$(release_manifest_max_bytes)
  manifest_bytes=$(command wc -c <"$manifest" | command awk '{print $1}') || return 1
  signature_bytes=$(command wc -c <"$signature" | command awk '{print $1}') || return 1
  [[ $manifest_bytes =~ ^[0-9]+$ && $signature_bytes =~ ^[0-9]+$ ]] || return 1
  (( manifest_bytes <= max_bytes && signature_bytes <= max_bytes )) || return 1
  verifier=$(type -P gpgv) || return 1
  [[ $verifier == /* && -f $verifier && ! -L $verifier ]] || return 1
  release_path_is_root_controlled "$verifier" || return 1

  # gpgv can return success for an expired or revoked key. Require exactly
  # one good signature and reject every unexpected status, including expiry
  # and revocation, rather than treating the exit code as sufficient.
  (
    set -o pipefail
    "$verifier" --quiet --status-fd 1 --keyring "$keyring" "$signature" "$manifest" 2>/dev/null |
      command awk '
        $1 != "[GNUPG:]" { bad=1; exit }
        $2 == "NEWSIG" && phase == 0 { phase=1; next }
        ($2 == "KEY_CONSIDERED" || $2 == "SIG_ID") && phase == 1 { next }
        $2 == "GOODSIG" && phase == 1 { phase=2; next }
        $2 == "VALIDSIG" && phase == 2 { phase=3; next }
        { bad=1; exit }
        END { if (bad || phase != 3) exit 1 }
      '
  )
}

# Return the digest of the same bounded regular manifest used by the trust
# decision. The caller carries this exact value to the transaction boundary.
release_signed_manifest_sha256() {
  local PATH=/usr/sbin:/usr/bin:/sbin:/bin
  local bundle=$1 manifest max_bytes manifest_bytes digest
  [[ $bundle == /* && -d $bundle && ! -L $bundle ]] || return 1
  release_path_is_root_controlled "$bundle" || return 1
  manifest="$bundle/SHA256SUMS"
  [[ -f $manifest && ! -L $manifest ]] || return 1
  release_path_is_root_controlled "$manifest" || return 1
  max_bytes=$(release_manifest_max_bytes)
  manifest_bytes=$(command wc -c <"$manifest" | command awk '{print $1}') || return 1
  [[ $manifest_bytes =~ ^[0-9]+$ ]] || return 1
  (( manifest_bytes <= max_bytes )) || return 1
  digest=$(command sha256sum -- "$manifest" | command awk '{print $1}') || return 1
  [[ $digest =~ ^[[:xdigit:]]{64}$ ]] || return 1
  printf '%s\n' "${digest,,}"
}

# Detect ordinary manifest changes across verification. Equal observations do
# not seal bytes against a writer able to replace or restore the file between
# reads; native verification must consume an immutable snapshot instead.
release_verified_manifest_sha256() {
  local bundle=$1 keyring=$2 before after
  before=$(release_signed_manifest_sha256 "$bundle") || return 1
  release_verify_signed_manifest "$bundle" "$keyring" || return 1
  after=$(release_signed_manifest_sha256 "$bundle") || return 1
  [[ $before == "$after" ]] || return 1
  printf '%s\n' "$after"
}

# Compare an attended, independently obtained digest with the exact selected
# SHA256SUMS file before invoking any payload code.
release_manifest_matches_approval() {
  local PATH=/usr/sbin:/usr/bin:/sbin:/bin
  local bundle=$1 approved=$2 manifest manifest_bytes actual max_bytes
  [[ $bundle == /* && -d $bundle && ! -L $bundle ]] || return 1
  [[ $approved =~ ^[[:xdigit:]]{64}$ ]] || return 1
  manifest="$bundle/SHA256SUMS"
  [[ -f $manifest && ! -L $manifest ]] || return 1
  max_bytes=$(release_manifest_max_bytes)
  manifest_bytes=$(command wc -c <"$manifest" | command awk '{print $1}') || return 1
  [[ $manifest_bytes =~ ^[0-9]+$ ]] || return 1
  (( manifest_bytes <= max_bytes )) || return 1
  actual=$(command sha256sum -- "$manifest" | command awk '{print $1}') || return 1
  [[ $actual == "${approved,,}" ]]
}
