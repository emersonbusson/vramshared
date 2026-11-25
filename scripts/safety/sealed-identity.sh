# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/sealed-identity.sh
# sealed-identity.sh — one acceptance predicate for sealed identity files.
# shellcheck shell=bash
#
# Shell counterpart of `crates/ramshared-cli/src/release_identity.rs`.
# Their byte and whitespace rules match. UTF-8 is checked with the declared
# `iconv` runtime dependency; the explicit Unicode boundary set mirrors Rust's
# `str::trim()` whitespace property.
#
# The rule, applied once:
#   1. the path is a regular file — a symlink is not a sealed identity file;
#   2. the content, with at most one trailing newline stripped, is non-empty;
#   3. it contains no embedded newline, carriage return or NUL;
#   4. it carries no surrounding whitespace.
#   5. it is at most 4096 bytes, including any trailing newline.
# A missing final newline is tolerated. Nothing is trimmed: a document the rule
# refuses is refused, never normalized into a match.
#
# Source from other scripts:
#   # shellcheck source=sealed-identity.sh
#   source "$SCRIPT_DIR/sealed-identity.sh"

# shellcheck disable=SC2034
SEALED_IDENTITY_LIB_LOADED=1

sealed_identity_is_regular_file() {
  [[ -f $1 && ! -L $1 ]]
}

# Match Rust `str::trim()` at the start or end of a valid UTF-8 line. Bash's
# locale character classes do not implement the same Unicode property across
# supported systems, so enumerate the non-ASCII White_Space code points here.
sealed_identity_has_unicode_boundary_whitespace() {
  local value=$1 whitespace
  for whitespace in \
    $'\302\205' $'\302\240' $'\341\232\200' \
    $'\342\200\200' $'\342\200\201' $'\342\200\202' \
    $'\342\200\203' $'\342\200\204' $'\342\200\205' \
    $'\342\200\206' $'\342\200\207' $'\342\200\210' \
    $'\342\200\211' $'\342\200\212' $'\342\200\250' \
    $'\342\200\251' $'\342\200\257' $'\342\201\237' \
    $'\343\200\200'; do
    [[ $value == "$whitespace"* || $value == *"$whitespace" ]] && return 0
  done
  return 1
}

# Prints the accepted line on stdout and returns 1 with no output on refusal.
sealed_identity_read_line() {
  local path=$1 line stripped probe LC_ALL=C
  sealed_identity_is_regular_file "$path" || return 1
  # Bash normally discards NUL bytes while reading into a variable. A NUL
  # delimiter makes it visible; the byte limit also bounds this probe.
  if IFS= read -r -d '' -n 4097 probe <"$path"; then
    return 1
  fi
  line=
  IFS= read -r line <"$path" || [[ -n $line ]] || return 1
  [[ -n $line ]] || return 1
  # The installed packages declare the provider of this validator. Refuse if
  # it is absent or if any byte sequence is not valid UTF-8; never normalize.
  command -v iconv >/dev/null 2>&1 || return 1
  if ! printf '%s' "$line" | LC_ALL=C iconv -f UTF-8 -t UTF-8 >/dev/null 2>&1; then
    return 1
  fi
  # Exactly one line: a second record must not exist, not even an empty one.
  # `wc -l` counts newlines (so a file with no final newline is 0 and
  # "0.15.0\n\n" is 2); `sed -n '2p'` catches a trailing record `wc` undercounts.
  [[ $(wc -l <"$path") -le 1 ]] || return 1
  [[ -z $(sed -n '2p' "$path") ]] || return 1
  # `line.trim() == line` — refuse surrounding whitespace instead of stripping.
  stripped=$line
  stripped=${stripped#"${stripped%%[![:space:]]*}"}
  stripped=${stripped%"${stripped##*[![:space:]]}"}
  [[ $stripped == "$line" ]] || return 1
  sealed_identity_has_unicode_boundary_whitespace "$line" && return 1
  [[ $line != *$'\r'* ]] || return 1
  printf '%s\n' "$line"
}
