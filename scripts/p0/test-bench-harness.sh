#!/usr/bin/env bash
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/p0/test-bench-harness.sh
# test-bench-harness.sh — functional contract of the registered benchmark harness.
#
# Covers the aggregator argv split: `bench.sh` passes 14 fixed arguments and
# then the caller's tool args after `--`. A hand-sized slice against a
# 14-tuple silently worked with no tool args and raised on any, so the
# documented invocation could not run. This test exercises both shapes plus
# the metric-less fallback.
#
# usage: test-bench-harness.sh
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
bench="$root/scripts/p0/bench.sh"
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT

results="$work/results.jsonl"
: >"$results"

# Stub tool: emits one results.jsonl-shaped line with a metrics map, echoing
# its args so the caller can prove the pass-through.
cat >"$work/stub-measure.sh" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
echo "stub-args:$*"
python3 - <<'PY'
import json
print(json.dumps({
    "run_id": "stub",
    "metrics": {
        "sample_metric": {"n": 2, "min": 1.0, "median": 1.5, "max": 2.0, "mean": 1.5, "stddev": 0.7},
        "per_round_sample_metric": [1.0, 2.0],
    },
}, separators=(",", ":")))
PY
STUB
chmod +x "$work/stub-measure.sh"

# Metric-less stub: forces the raw_output_bytes fallback.
cat >"$work/stub-quiet.sh" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
echo "quiet"
STUB
chmod +x "$work/stub-quiet.sh"

fail() { echo "FAIL $*" >&2; exit 1; }

# 1. With tool args — the shape that used to raise in the aggregator.
out=$(bash "$bench" --tool "$work/stub-measure.sh" --run-id t-args \
    --condition idle --rounds 3 --title 'stub with args' \
    --results "$results" --dry-run -- --drill snapshot --json 2>&1) \
    || fail "harness_with_tool_args_exited_nonzero"
grep -q 'sample_metric' <<<"$out" || fail "aggregator_did_not_pool_metrics"
grep -Fq 'args=`--drill snapshot --json`' <<<"$out" \
    || fail "tool_args_not_recorded_in_paste_block"
echo "PASS harness_with_tool_args"

# 2. Without tool args — the shape that already worked, must keep working.
out=$(bash "$bench" --tool "$work/stub-measure.sh" --run-id t-noargs \
    --condition idle --rounds 3 --title 'stub without args' \
    --results "$results" --dry-run 2>&1) \
    || fail "harness_without_tool_args_exited_nonzero"
grep -Fq 'args=`(none)`' <<<"$out" || fail "empty_tool_args_not_rendered"
echo "PASS harness_without_tool_args"

# 3. A round with no metric must still produce a non-empty row.
out=$(bash "$bench" --tool "$work/stub-quiet.sh" --run-id t-quiet \
    --condition idle --rounds 3 --title 'stub with no metric' \
    --results "$results" --dry-run 2>&1) \
    || fail "metricless_round_exited_nonzero"
grep -q 'raw_output_bytes' <<<"$out" || fail "metricless_fallback_missing"
echo "PASS metricless_round_falls_back_to_raw_bytes"

# 4. A real append lands exactly one machine row and it is parseable.
bash "$bench" --tool "$work/stub-measure.sh" --run-id t-append \
    --condition idle --rounds 3 --title 'stub append' \
    --results "$results" -- \
    --drill snapshot --json >/dev/null 2>&1 \
    || fail "append_run_exited_nonzero"
rows=$(wc -l <"$results")
[ "$rows" -eq 1 ] || fail "expected_exactly_one_appended_row got $rows"
python3 - "$results" <<'PY' || fail "appended_row_not_parseable"
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1], encoding="utf-8") if l.strip()]
assert len(rows) == 1, rows
row = rows[0]
assert row["run_id"] == "t-append", row["run_id"]
assert row["parameters"]["tool_args"] == ["--drill", "snapshot", "--json"], row["parameters"]
assert "sample_metric" in row["metrics"], sorted(row["metrics"])
assert row["publication_status"] == "legacy-unqualified", row["publication_status"]
PY
echo "PASS appends_one_parseable_row_with_tool_args"

# 5. The argv split is named, not a hand-sized slice — the shape that drifted.
#    Matched as a pattern so a prose comment cannot trip the gate.
python3 - "$bench" <<'PY' || fail "fixed_arg_split_is_not_named"
from pathlib import Path
import re
import sys
src = Path(sys.argv[1]).read_text(encoding="utf-8")
assert "FIXED_ARGS = 14" in src, "named_fixed_arg_count_missing"
assert "sys.argv[1:1 + FIXED_ARGS]" in src, "fixed_slice_missing"
assert "sys.argv[1 + FIXED_ARGS:]" in src, "tool_arg_slice_missing"
hand_sized = re.findall(r"sys\.argv\[\d+:\d*\]", src)
assert not hand_sized, f"hand_sized_slice_regression {hand_sized}"
PY
echo "PASS fixed_arg_split_is_named_not_hand_sized"

echo "BENCH_HARNESS=PASS"
