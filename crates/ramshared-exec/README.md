# ramshared-exec
<!--
Author: Emerson Busson
Email: emersonbusson@gmail.com
GitHub: https://github.com/emersonbusson
LinkedIn: https://www.linkedin.com/in/emersonbusson
Source: https://github.com/emersonbusson/vramshared
File: crates/ramshared-exec/README.md
-->

Shared execution boundary for trusted, short-lived RamShared subprocesses.
The CLI, local observation service and serving daemon share native process
effects and bounded capture. Controller policy is in
[`bounded_process.rs`](src/bounded_process.rs); serving-process custody is in
[`retained_process.rs`](src/retained_process.rs).

## Scope & Responsibility

The Unix/Linux runner starts the child as leader of an invocation-owned process
group. It retains custody during timeout, output overflow, inherited-pipe
failure and panic. Linux exit observation pins the unreaped leader while exact
group cleanup closes descendant-held pipes, then reaps the direct child.
Cleanup uses the owned group, not executable names or broad PID searches.

Helpers must be trusted and non-daemonizing. A helper that deliberately escapes
the group with setsid/group changes is outside this contract. This is not a
Windows process/service custody implementation or a sandbox for arbitrary code.

## Workspace Dependencies

No direct workspace-crate dependency. The crate uses `rustix` process APIs and
the Rust standard library; the CLI, local observation service and daemon are consumers. The workspace
lockfile owns the external dependency version. The subprocess execution owner
does not import caller policy, device lifecycles or customer command routing.

## Public interface

- `run_capture_command`: argv-based execution with a caller deadline, bounded
  stdout/stderr capture and spawn notification; returns typed status/output or
  `ProcessSpawnError`. The output limit applies to each stream; callers needing
  a combined budget must account for both.
- `configure_process_group`: configures the owned process-group boundary.
- `wait_grouped_child`: bounded wait and owned cleanup for an already started
  grouped child.
- `terminate_group_and_reap`: explicit owned group/direct-child cleanup.
- `BoundedOutput`, `ProcessSpawnError`, `DEFAULT_OUTPUT_LIMIT`: result/error
  contract and default capture bound (64 KiB per stream in current source).

The caller validates program, argv, environment, privileges and permitted
effects; a timeout does not make a privileged operation authorized. Failure to
prove termination/reaping selects fatal controller containment rather than
returning to ordinary control flow with an unowned child.

The serving daemon uses `retained_process::ServingCommandOwner` with a
process-lifetime borrow. Pending work retains the actual child and capture,
refuses new admission and blocks shutdown. Reconciliation observes the same
operation. Numeric group IDs are never signalled after reaping; later group
absence checks are read-only. A `HelperShutdownLease` freezes admission through
the last backend owner. A deadline does not prove kernel cancellation or bound
arbitrary synchronous filesystem/kernel calls.

## Safety Invariants

Use this boundary for disposable observations or other operations whose exact
group may safely be killed. Do not place active storage daemons, swapoff,
pagefile service owners or ongoing GPU-resource custody behind its deadline
cleanup. Those operations require their lifecycle's retained backing and safe
drain/recovery contract; a deadline does not permit freeing live resources.

## Testing

The source suite includes typed success/nonzero status, output overflow,
malformed argv, panic cleanup, descendant-held pipes and fatal containment:

- `capture_runner_keeps_legitimate_success_and_nonzero_status_typed`
- `capture_runner_rejects_bounded_output_overflow`
- `capture_runner_reaps_successful_leader_and_all_stdio_redirected_descendant`
- `capture_runner_on_spawn_panic_cannot_strand_owned_child`
- `unreaped_group_selects_fatal_controller_containment`

Listing tests here is not a fresh execution or coverage claim. The customer
installation proposal uses this owner only for bounded observations; see
[Documentation/security/trust-model.rst](../../Documentation/security/trust-model.rst).
