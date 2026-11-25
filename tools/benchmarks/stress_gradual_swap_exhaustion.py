#!/usr/bin/env python3
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: tools/benchmarks/stress_gradual_swap_exhaustion.py
"""
tools/benchmarks/stress_gradual_swap_exhaustion.py
--------------------------------------------------
Bounded micro-step memory ramp for supervised qualification campaigns.

This is NOT a standalone pressure entrypoint. It is a lab tool that runs only
behind the same admission gate as the approved shared-host campaign, and it
reports a measured outcome rather than a fixed success string.

AUD-30 contracts, enforced here:

1. Admission is fail-closed. Without the supervised-campaign tokens the tool
   refuses to allocate anything and exits non-zero.
2. Pressure telemetry is mandatory. A missing or unparsable PSI source is an
   admission failure, never a silent zero that looks like "no pressure".
3. Containment is real. A watchdog thread owns the chunk list and drops it if
   the main loop stops checking in. The previous release claimed this thread
   and never created one.
4. Allocation is bounded and incremental. Chunks are built in micro-slices, so
   a single allocation step cannot take the host from idle to exhausted.
5. The outcome is derived. `PASS_ZERO_PANIC` is printed only when the ramp and
   the post-release measurements both completed. Abort and failure exit
   non-zero with a PARTIAL/ABORT verdict.
"""

import argparse
import os
import signal
import sys
import threading
import time
from typing import Dict, List, Optional, Tuple

# Same admission contract as scripts/safety/wsl2-freeze-campaign.sh. These are
# not bypasses: they are proof that a supervised campaign armed containment
# before this tool was invoked. Do not hand-export them on a daily host.
REQUIRED_SHARED_HOST_APPROVAL = "I_ACCEPT_WSL_TERMINATION"
REQUIRED_WATCHDOG_ARMED = "1"

MICRO_SLICE_MIB = 50
WATCHDOG_CHECKIN_TIMEOUT_SEC = 5.0
WATCHDOG_POLL_SEC = 0.5


class SafetyGovernorAbort(Exception):
    """Raised when a safety floor or an admission check stops the ramp."""


class TelemetryUnavailable(SafetyGovernorAbort):
    """Raised when a mandatory pressure source is missing or malformed."""


def read_meminfo() -> Dict[str, int]:
    """Reads /proc/meminfo in KiB. Missing or malformed input raises."""
    stats: Dict[str, int] = {}
    with open("/proc/meminfo", "r", encoding="utf-8") as handle:
        for line in handle:
            parts = line.strip().split(":")
            if len(parts) != 2:
                continue
            key = parts[0].strip()
            fields = parts[1].strip().split()
            if not fields:
                continue
            try:
                stats[key] = int(fields[0])
            except ValueError as error:
                raise TelemetryUnavailable(
                    f"meminfo field {key!r} is not an integer"
                ) from error
    if "MemTotal" not in stats or "MemAvailable" not in stats:
        raise TelemetryUnavailable("meminfo is missing MemTotal/MemAvailable")
    return stats


def read_swaps() -> List[Dict[str, object]]:
    """Reads /proc/swaps in KiB. Unreadable input raises."""
    tiers: List[Dict[str, object]] = []
    with open("/proc/swaps", "r", encoding="utf-8") as handle:
        lines = handle.readlines()
    for line in lines[1:]:
        parts = line.strip().split()
        if len(parts) < 5:
            continue
        try:
            tiers.append(
                {
                    "filename": parts[0],
                    "type": parts[1],
                    "size_kib": int(parts[2]),
                    "used_kib": int(parts[3]),
                    "priority": int(parts[4]),
                }
            )
        except ValueError as error:
            raise TelemetryUnavailable(
                f"swaps line is malformed: {line.strip()!r}"
            ) from error
    return tiers


def read_psi_memory() -> Tuple[float, float, int]:
    """Reads /proc/pressure/memory -> (some_avg10, full_avg10, full_total_us).

    The source is mandatory. A missing file, an unreadable file, or a line
    without the expected keys raises TelemetryUnavailable instead of returning
    zeros that would look like an idle host.
    """
    path = "/proc/pressure/memory"
    if not os.path.exists(path):
        raise TelemetryUnavailable("PSI memory source is not available")
    some_avg10 = None
    full_avg10 = None
    full_total_us = None
    with open(path, "r", encoding="utf-8") as handle:
        for line in handle:
            fields = line.split()
            if not fields:
                continue
            kind = fields[0]
            if kind not in ("some", "full"):
                continue
            for part in fields[1:]:
                if part.startswith("avg10="):
                    try:
                        value = float(part.split("=", 1)[1])
                    except ValueError as error:
                        raise TelemetryUnavailable(
                            f"PSI {kind} avg10 is not a float: {part!r}"
                        ) from error
                    if kind == "some":
                        some_avg10 = value
                    else:
                        full_avg10 = value
                elif part.startswith("total=") and kind == "full":
                    try:
                        full_total_us = int(part.split("=", 1)[1])
                    except ValueError as error:
                        raise TelemetryUnavailable(
                            f"PSI full total is not an integer: {part!r}"
                        ) from error
    if some_avg10 is None or full_avg10 is None or full_total_us is None:
        raise TelemetryUnavailable("PSI memory source is missing required keys")
    return some_avg10, full_avg10, full_total_us


def probe_allocation_latency_ms() -> float:
    """Probes one-page allocation and dirty latency in milliseconds.

    A failed probe raises. A silent zero here would look like an impossibly
    fast host and would disable the latency safety floor.
    """
    start = time.perf_counter()
    page = bytearray(4096)
    page[0] = 1
    page[4095] = 2
    del page
    return (time.perf_counter() - start) * 1000.0


class ContainmentWatchdog:
    """Owns the live allocation list and drops it if the ramp stops checking in.

    This is the containment the previous release advertised and did not
    implement. It is independent of the allocating thread: the ramp must call
    `check_in()` at least every `checkin_timeout_sec`, or the watchdog clears
    the chunks and records the stall.
    """

    def __init__(
        self,
        chunks: List[bytearray],
        checkin_timeout_sec: float = WATCHDOG_CHECKIN_TIMEOUT_SEC,
        poll_sec: float = WATCHDOG_POLL_SEC,
    ) -> None:
        self._chunks = chunks
        self._timeout = checkin_timeout_sec
        self._poll = poll_sec
        self._last_checkin = time.monotonic()
        self._lock = threading.Lock()
        self._stop = threading.Event()
        self._thread = threading.Thread(
            target=self._run, name="ramshared-pressure-watchdog", daemon=True
        )
        self.triggered = False
        self.trigger_reason = ""
        self.released_bytes = 0

    def start(self) -> None:
        self._thread.start()

    def check_in(self) -> None:
        with self._lock:
            self._last_checkin = time.monotonic()

    def stop(self) -> None:
        self._stop.set()
        self._thread.join(timeout=self._poll * 4)

    def _run(self) -> None:
        while not self._stop.wait(self._poll):
            with self._lock:
                overdue = (time.monotonic() - self._last_checkin) > self._timeout
            if overdue:
                self._fire("main_loop_missed_checkin")
                return

    def _fire(self, reason: str) -> None:
        self.triggered = True
        self.trigger_reason = reason
        released = 0
        while self._chunks:
            chunk = self._chunks.pop()
            released += len(chunk)
        self.released_bytes = released


def admit(args: argparse.Namespace) -> None:
    """Fail-closed admission. Raises SafetyGovernorAbort when not supervised."""
    reasons: List[str] = []
    if not args.allow_isolated_lab:
        reasons.append("daily_host_refused_without_isolated_lab_flag")
    if os.environ.get("RAMSHARED_SHARED_HOST_APPROVAL") != REQUIRED_SHARED_HOST_APPROVAL:
        reasons.append("shared_host_approval_missing_or_mismatched")
    if os.environ.get("RAMSHARED_WINDOWS_WATCHDOG_ARMED") != REQUIRED_WATCHDOG_ARMED:
        reasons.append("windows_watchdog_not_armed")
    if reasons:
        raise SafetyGovernorAbort("admission_refused: " + ",".join(reasons))

    # Mandatory telemetry, checked before any allocation. A missing PSI source
    # is a refusal, not permission to ramp on zeros.
    read_psi_memory()
    read_meminfo()


def allocate_bounded(total_bytes: int, checkin) -> bytearray:
    """Build one chunk in bounded micro-slices so no single step can exhaust."""
    chunk = bytearray()
    remaining = total_bytes
    while remaining > 0:
        step = min(remaining, MICRO_SLICE_MIB * 1024 * 1024)
        chunk.extend(bytearray(step))
        remaining -= step
        checkin()
    return chunk


def derive_outcome(
    completed_ramp: bool,
    measured_reclaim: bool,
    abort_reason: str,
    failure: Optional[str],
) -> Tuple[str, int]:
    """Map run state to (verdict, exit_code). No unconditional PASS."""
    if failure:
        return ("ABORT_FAILED", 1)
    if abort_reason:
        return ("ABORT_SAFETY", 2)
    if not completed_ramp:
        return ("PARTIAL_RAMP_INCOMPLETE", 3)
    if not measured_reclaim:
        return ("PARTIAL_RECLAIM_UNMEASURED", 4)
    return ("QUALIFIED_PASS", 0)


class RampState:
    """Mutable run counters. One place owns them so main stays a controller."""

    def __init__(self) -> None:
        self.completed_ramp = False
        self.measured_reclaim = False
        self.abort_reason = ""
        self.failure: Optional[str] = None
        self.total_allocated_mb = 0
        self.peak_zram_mb = 0
        self.peak_vram_mb = 0
        self.peak_ssd_mb = 0
        self.peak_total_swap_mb = 0
        self.max_safe_pct_reached = 0
        self.released_bytes = 0
        self.post_free_ram_mb = 0
        self.post_swap_used_mb = 0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Bounded micro-step memory ramp for supervised campaigns"
    )
    parser.add_argument("--start-pct", type=int, default=1)
    parser.add_argument("--max-target-pct", type=int, default=95)
    parser.add_argument("--step-pct", type=int, default=1)
    parser.add_argument("--step-interval-sec", type=float, default=2.0)
    parser.add_argument("--hold-peak-sec", type=float, default=15.0)
    parser.add_argument("--min-free-ram-mb", type=int, default=600)
    parser.add_argument("--max-psi-full", type=float, default=20.0)
    parser.add_argument("--max-latency-ms", type=float, default=8.0)
    parser.add_argument("--max-psi-stall-rate-ms", type=float, default=50.0)
    parser.add_argument(
        "--allow-isolated-lab",
        action="store_true",
        help="Required. Refuses to run without it; pairs with the campaign tokens.",
    )
    return parser.parse_args()


def classify_swaps(swaps_post: List[Dict[str, object]]) -> Tuple[int, int, int]:
    """Split /proc/swaps occupancy into zram / nbd / fallback, in MB.

    The third bucket is the last-resort fallback swap device. It is the only
    quantity the fallback-spill alarm applies to; see the canonical metric
    table in security and benchmark guidelines
    """
    z_used = v_used = s_used = 0
    for entry in swaps_post:
        name = str(entry["filename"])
        used_kib = int(entry["used_kib"])
        if "zram" in name:
            z_used += used_kib
        elif "nbd" in name:
            v_used += used_kib
        else:
            s_used += used_kib
    return ((z_used + 512) // 1024, (v_used + 512) // 1024, (s_used + 512) // 1024)


def gate_floor_breach(args: argparse.Namespace, avail_mb: float, full_psi: float,
                      lat_ms: float, stall_rate_ms: float) -> Optional[str]:
    """Return the abort reason when a safety floor is crossed, else None."""
    if avail_mb <= args.min_free_ram_mb:
        return f"ram_floor avail={avail_mb} floor={args.min_free_ram_mb}"
    if full_psi >= args.max_psi_full:
        return f"psi_full_limit value={full_psi:.1f} limit={args.max_psi_full}"
    if lat_ms >= args.max_latency_ms:
        return f"latency_limit value={lat_ms:.2f} limit={args.max_latency_ms}"
    if stall_rate_ms > args.max_psi_stall_rate_ms:
        return (
            f"psi_stall_rate_limit value={stall_rate_ms:.2f} "
            f"limit={args.max_psi_stall_rate_ms}"
        )
    return None


def announce_host() -> int:
    """Print the host envelope and return physical RAM in MB."""
    mem_init = read_meminfo()
    ram_total_mb = (mem_init.get("MemTotal", 0) + 512) // 1024
    swap_total_mb = (mem_init.get("SwapTotal", 0) + 512) // 1024
    print(
        f"[i] Host Physical RAM: {ram_total_mb} MB "
        f"| Swap Pool: {swap_total_mb} MB "
        f"(supervised campaign admission accepted)"
    )
    return ram_total_mb


def hold_peak(
    args: argparse.Namespace,
    state: RampState,
    watchdog: "ContainmentWatchdog",
) -> None:
    """Keep the peak resident for the hold window, checking in every second."""
    hold_end = time.time() + args.hold_peak_sec
    while time.time() < hold_end and not state.abort_reason:
        watchdog.check_in()
        if watchdog.triggered:
            break
        time.sleep(1.0)


def run_ramp(
    args: argparse.Namespace,
    state: RampState,
    chunks: List[bytearray],
    watchdog: "ContainmentWatchdog",
) -> None:
    """Drive one supervised micro-step ramp. Mutates `state` in place."""
    max_target_pct = min(max(args.max_target_pct, 1), 99)
    step_pct = max(1, args.step_pct)

    def checkin() -> None:
        watchdog.check_in()

    def sig_handler(signum, _frame) -> None:
        state.abort_reason = f"signal_{int(signum)}"
        while chunks:
            state.released_bytes += len(chunks.pop())

    signal.signal(signal.SIGINT, sig_handler)
    signal.signal(signal.SIGTERM, sig_handler)

    ram_total_mb = announce_host()

    _some_psi, _full_psi, last_full_total_us = read_psi_memory()
    current_target = args.start_pct

    while current_target <= max_target_pct:
        checkin()
        if state.abort_reason:
            break

        mem_pre = read_meminfo()
        avail_mb = (mem_pre.get("MemAvailable", 0) + 512) // 1024
        _some_psi, full_psi, full_total_us = read_psi_memory()
        lat_ms = probe_allocation_latency_ms()
        stall_rate_ms = max(0, full_total_us - last_full_total_us) / 1000.0
        last_full_total_us = full_total_us

        breach = gate_floor_breach(args, avail_mb, full_psi, lat_ms, stall_rate_ms)
        if breach:
            state.abort_reason = breach
            break

        one_pct_chunk_mb = max(50, (ram_total_mb * step_pct) // 100)
        safe_alloc_mb = min(
            one_pct_chunk_mb, max(0, avail_mb - args.min_free_ram_mb)
        )
        if safe_alloc_mb <= 0:
            state.abort_reason = "no_safe_headroom"
            break

        chunk = allocate_bounded(safe_alloc_mb * 1024 * 1024, checkin)
        for offset in range(0, len(chunk), 4096):
            chunk[offset] = (current_target + offset) & 0xFF
        chunks.append(chunk)
        state.total_allocated_mb += safe_alloc_mb

        mem_post = read_meminfo()
        swaps_post = read_swaps()
        _some_psi, full_psi, _total = read_psi_memory()
        lat_ms = probe_allocation_latency_ms()
        checkin()

        swap_used_mb = (
            mem_post.get("SwapTotal", 0) - mem_post.get("SwapFree", 0) + 512
        ) // 1024
        state.peak_total_swap_mb = max(state.peak_total_swap_mb, swap_used_mb)

        z_mb, v_mb, s_mb = classify_swaps(swaps_post)
        state.peak_zram_mb = max(state.peak_zram_mb, z_mb)
        state.peak_vram_mb = max(state.peak_vram_mb, v_mb)
        state.peak_ssd_mb = max(state.peak_ssd_mb, s_mb)
        state.max_safe_pct_reached = current_target

        print(
            f"level={current_target}% alloc={state.total_allocated_mb}MB "
            f"zram={z_mb}MB vram={v_mb}MB fallback_swap={s_mb}MB "
            f"psi_full={full_psi:.1f}% stall_rate={stall_rate_ms:.2f}ms "
            f"lat={lat_ms:.2f}ms"
        )

        time.sleep(args.step_interval_sec)
        current_target += step_pct

    if state.abort_reason or watchdog.triggered:
        return

    state.completed_ramp = current_target > max_target_pct
    hold_peak(args, state, watchdog)


def write_report(state: RampState, watchdog: "ContainmentWatchdog",
                 verdict: str, code: int) -> None:
    """Emit the measured outcome. No unconditional success string exists."""
    print("---- outcome ----")
    print(f"verdict: {verdict}")
    print(f"exit_code: {code}")
    print(f"max_level_reached_pct: {state.max_safe_pct_reached}")
    print(f"peak_allocated_mb: {state.total_allocated_mb}")
    print(f"peak_zram_mb: {state.peak_zram_mb}")
    print(f"peak_vram_mb: {state.peak_vram_mb}")
    print(f"peak_fallback_ssd_swap_mb: {state.peak_ssd_mb}")
    print(f"peak_total_swap_mb: {state.peak_total_swap_mb}")
    print(f"released_bytes: {state.released_bytes}")
    print(f"post_free_ram_mb: {state.post_free_ram_mb}")
    print(f"post_swap_used_mb: {state.post_swap_used_mb}")
    print(f"watchdog_triggered: {watchdog.triggered}")
    if state.abort_reason:
        print(f"abort_reason: {state.abort_reason}")
    if state.failure:
        print(f"failure: {state.failure}")
    if verdict == "QUALIFIED_PASS":
        # Reserved for measured campaign results. An abort or an unmeasured
        # reclaim never reaches this line.
        print("PASS_ZERO_PANIC: asserted only for the measured window above")


def main() -> int:
    args = parse_args()
    try:
        admit(args)
    except SafetyGovernorAbort as error:
        print(f"[!] REFUSED: {error}", file=sys.stderr)
        print(
            "[!] Use the supervised campaign path "
            "(scripts/safety/wsl2-freeze-campaign.sh or "
            "scripts/windows/Invoke-SharedWslPressureCampaign.ps1) "
            "instead of invoking this ramp directly.",
            file=sys.stderr,
        )
        verdict, code = derive_outcome(False, False, str(error), None)
        print(f"[!] Outcome: {verdict}", file=sys.stderr)
        return code

    state = RampState()
    chunks: List[bytearray] = []
    watchdog = ContainmentWatchdog(chunks)
    watchdog.start()

    try:
        run_ramp(args, state, chunks, watchdog)
    except SafetyGovernorAbort as error:
        state.abort_reason = str(error)
    except Exception as error:  # noqa: BLE001 - classified, never silent
        state.failure = f"{type(error).__name__}: {error}"
    finally:
        # Reclaim before stopping the watchdog so containment still covers the
        # last window. The watchdog is stopped only after the list is empty.
        while chunks:
            state.released_bytes += len(chunks.pop())
        watchdog.stop()

    if watchdog.triggered:
        state.failure = state.failure or (
            f"watchdog_triggered: {watchdog.trigger_reason}"
        )
        state.released_bytes = max(state.released_bytes, watchdog.released_bytes)

    mem_final = read_meminfo() if state.failure is None else {}
    state.post_free_ram_mb = (mem_final.get("MemAvailable", 0) + 512) // 1024
    state.post_swap_used_mb = (
        mem_final.get("SwapTotal", 0) - mem_final.get("SwapFree", 0) + 512
    ) // 1024

    if state.failure is None and state.abort_reason is None and state.completed_ramp:
        # Reclaim was measured: the chunks are gone and the host telemetry
        # moved. Only then is a qualified verdict permitted.
        state.measured_reclaim = (
            state.released_bytes > 0 and state.post_free_ram_mb > 0
        )

    verdict, code = derive_outcome(
        state.completed_ramp,
        state.measured_reclaim,
        state.abort_reason,
        state.failure,
    )
    write_report(state, watchdog, verdict, code)
    return code


if __name__ == "__main__":
    sys.exit(main())
