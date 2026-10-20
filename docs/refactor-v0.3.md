# Reach v0.3 implementation and acceptance record

This is an in-progress refactor, not a release completion claim. The installed
production service has not been replaced. Backward compatibility is not a goal.

## Implemented direction

- `operation` owns the operation registry and Rust argument types. Schemars
  generates native discovery; the same schema generates initial signatures.
  Both MCP profiles decode to the same typed `Operation` and executor.
- `error` defines stable codes and recovery guidance. Invalid submissions are
  distinct from durable asynchronous task outcomes.
- `jobs` has queued/running/terminal states and notification-based completion
  waits. Retry fingerprints persist. Expiration keeps an identity tombstone,
  rather than silently allowing the same ID to execute again.
- `changes` owns file plans and per-file receipts. Existing-file replacement,
  append and edit require versions. Windows commits hold exclusive handles
  through writes and rollback. This prevents a second normal filesystem writer
  from changing the same file during a commit. It deliberately does not claim
  crash-atomic writes or a multi-file transaction. Parent-directory creation is
  not rolled back. Non-Windows locks are advisory and are not yet verified.
- `terminal` owns explicit PTY resources independently of command tasks and MCP
  transport connections. `terminal_output` continuously drains bytes into a
  bounded ring and updates a VT screen. Unchanged screen hashes suppress redraw
  repetition. Raw stream reads use base64 and byte cursors, with explicit gaps.
- `windows_pty` uses ConPTY and a kill-on-close Job Object assigned by the
  process-creation attribute list, before the suspended main thread is resumed.
  This closes the post-spawn ownership window of the initial portable backend.

## Windows runtime

The Windows 11 22631 system ConPTY was observed to flatten alternate-screen
switches. Microsoft's redistributable ConPTY fixes that observed behavior.
It is loaded only by an absolute path beside the executable, never from cwd.
If absent, the system API is used; the full alternate-screen acceptance test
requires the prepared runtime. Runtime capability reporting still needs work.

The pinned package is Microsoft.Windows.Console.ConPTY 1.25.260930003, MIT,
from [NuGet](https://www.nuget.org/packages/Microsoft.Windows.Console.ConPTY/1.25.260930003).
The preparation script checks package/file hashes and Microsoft Authenticode
signatures. DLL and host executable remain external release assets, not source.

```powershell
# Substitute the actual cargo target directory; cargo metadata reports it.
./scripts/prepare-windows-runtime.ps1 -Destination <target>/debug/deps
./scripts/prepare-windows-runtime.ps1 -Destination <target>/debug
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

Validated on the current Windows machine: separate agents' persistent cwd/env,
real console handles, Unicode, control-key input, actual PTY dimensions, input
retry identities, idle expiration, retained metadata, native/compact HTTP
ownership enforcement, alternate-screen entry/exit, unchanged screen suppression
and raw ring overflow/gaps. These are actual child processes, not PTY mocks.

## Outstanding acceptance work (goal remains active)

1. Finish typed results and remove the read/search/image JSON adapter, dynamic
   task result payloads and remaining string-based errors. Improve generated
   signatures for nested/nullable schemas. Consolidate mutation registration.
2. Preserve mixed line endings and UTF-8 BOM during contextual patches; bound
   cumulative diff output; add durable pre-write recovery material for crash
   interruption. Exercise precise partial-write and rollback failures, including
   a failed rollback and concurrent external Windows writers.
3. Bound total retained task results/logs, run periodic cleanup and notify output
   waiters. Test queued cancellation, expiration without replay, unknown durable
   outcomes and service crash/restart from a real isolated process.
4. Extend PTY verification to descendants, forced executor death, startup/close
   races, quota rejection and blocked input queues. Report bundled/system backend
   capabilities truthfully. Verify invalid cwd/env/program paths and runtime
   startup failures. Review long-running session and reader/writer shutdown races.
5. Review module boundaries after the final model stabilizes; update public docs,
   preparation/release packaging, privacy scan and end-to-end agent workflows.
   Only then produce artifacts and assess production cutover without disturbing
   existing user sessions or transport links.
