# Reach v0.3 acceptance record

Scope: the full core/file/task/terminal refactor. Compatibility with v0.2 is
intentionally broken. Installed production Reach, its current TUI sessions, SSH
forwarding and VPN configuration have not been replaced or restarted.

## Requirement evidence

| Requirement | Authoritative implementation | Targeted verification |
|---|---|---|
| One operation contract | operation registry generates argument decoding, permission map, schema, initial signatures, typed replies and dispatch; engine has typed handlers | generated_guide_resolves_nested_sum_types_from_the_actual_schema; protocol compact/native tests |
| Typed parameters, results, errors and states | operation, results, error, jobs lifecycle macro | invalid/missing tagged arguments; stable HTTP error-code assertions; durable phase tests |
| Create / replace / append / edit | WriteContent tagged sum and file planners; shared version checks | create_append_replace_and_exact_edit_have_distinct_versioned_semantics |
| Version conflicts / external writers | changes exclusive Windows handles and recheck | windows_commit_excludes_external_writes_and_rename_until_handles_close; conflict rollback test |
| Preview / summaries / partial failures | bounded per-file receipts, global diff budget, all-file planning | multi_file_diff_output_has_a_global_budget; failed_rollback_is_reported_per_file_and_does_not_claim_restoration |
| Encoding / CRLF / ambiguity | UTF-8 validation; BOM-aware reads; per-line patch ending retention | mixed_line_endings_bom_and_missing_final_newline_survive_contextual_edits; unique context/hash/move/delete tests |
| File crash recovery | durable before-images before workspace writes; explicit guarded Recover | actual_executor_death_between_files_leaves_recoverable_before_images starts a real child and force-kills it after the first file commits; restoration with current versions |
| Task lifecycle / cancellation / timeout | terminal-only completion type, queued/running phases, Job Objects, independent deadline | durable_retry_identity_ownership_cancel_and_restart_never_replay; Windows deadline and descendant cancellation tests |
| Wait notifications / output cursors | subscription-before-observation; Poll wakes on new bytes; Tasks waits for completion | output_wait_wakes_on_new_bytes_and_utf8_pages_keep_source_cursors; running output/stdin test |
| Retention / quotas / replay prevention | bounded payloads; periodic cleanup; persistent identity tombstones | expiration_bounds_logs_and_recovery_but_retains_retry_tombstones; failed durable save/restart test |
| Distinct persistent terminal model | terminal manager separate from Jobs; principal ownership | terminal_ownership_is_enforced_over_both_mcp_profiles |
| True Windows PTY / cwd / env / interactive | native ConPTY plus process-creation Job Object attributes | windows_real_pty_persistent_shell_resize_unicode_and_independent_agents |
| Control keys / resize / input retry | semantic input identity, terminal-aware key encoding, actual OS resize | windows_pty_interactive_keys_input_retry_gaps_idle_and_restart |
| Quotas / idle / descendants / close race | bounded manager, idle reaper, kill-on-close tree, lifecycle lock | windows_pty_quota_bad_start_retention_and_descendants_are_bounded; windows_pty_concurrent_open_close_never_resurrects_a_closed_session |
| Backpressure / no unbounded writer queue | 32-message queue and 8 KiB input cap, delivery outcomes | windows_pty_blocked_input_queue_reports_backpressure_and_close_releases_waiters |
| Unicode / VT / cursor / alt screen | VT parser with bounded raw ring, separate reader/writer/exit handling | unicode_cursor_alternate_screen_and_overflow_are_explicit; real fullscreen VT entry/exit and redraw suppression test |
| Disconnect / restart / real OS process death | durable ownership and identities; interrupted state; no automatic reopening | scripts/test-service-recovery.cjs against an isolated executable: actual independent HTTP requests, forced service death, descendant termination, no command replay, file restore after restart |
| Packaging / runtime capability | pinned signed Microsoft ConPTY, absolute-path loading, Info backend report | prepare-windows-runtime.ps1; build-windows-release.ps1; portable-package service acceptance |

The ignored crash_during_commit_fixture is not an omitted acceptance case. Its
parent test launches it with --ignored in a separate process, synchronizes on the
first real committed file, force-kills that child and verifies the journal and
workspace. Running the fixture directly without its parent is unsupported.

## Explicit guarantees and limits

- Windows file writes are exclusive during ordinary commit/rollback. They are
  not single-file or multi-file crash-atomic. Parent directories are not rolled
  back. Recovery requires retained before-images and explicit current versions.
- Interrupted and unknown work never replays. Without a caller-provided initial
  operation_id, a lost submission response remains ambiguous; inspect task lists.
- Payload expiration preserves identity tombstones. A full identity ledger rejects
  new work instead of erasing retry safety; choose a new namespace deliberately.
- PTY output is one mixed byte stream. Separate stdout/stderr apply to Shell only.
  Screen versions reflect visible text/cursor/modes; raw bytes remain available.
- Terminal output rings continuously drain and evict; gaps identify lost retained
  bytes. Ended output expires after age/count limits; restart expires all PTY
  output. Existing terminals become interrupted, not recreated.
- A delivered input receipt confirms only OS-channel writing. Partial/unknown
  delivery requires inspection. The input retry ledger belongs to that PTY and
  is never replayed across process restart.
- Complete alternate-screen acceptance uses bundled Microsoft ConPTY. The older
  system ConPTY on this Windows build was observed to flatten those sequences;
  Info reports that backend's reduced capability truthfully.
- Non-Windows behavior is not production-validated. File locks there are advisory;
  no equivalent Unix process-tree guarantee is claimed.
- API roots and principal ownership are not OS isolation. exec is the account's
  execution authority. No host prompt caching or exact-once network guarantee is
  claimed.

## Validation gates

Current source gates: cargo fmt --check, cargo clippy --locked --all-targets --
-D warnings, cargo test --locked. Windows runtime package hashes and Microsoft
Authenticode signatures are checked by the preparation script. Portable ZIP
contents, runtime execution and privacy scanning are verified before delivery.

On the verified Windows run, 39 tests passed, with the child-only crash fixture
listed as ignored in the parent run and explicitly executed by its passing parent.
The isolated portable release process test also passed: independent HTTP reads,
forced executor death, complete PTY descendant cleanup, interrupted terminal/task
recovery, no command replay and guarded file restoration from the durable journal.
The privacy checks found no credentials/private keys in source or release inputs.

Production cutover is intentionally separate from acceptance: a v0.3 service
cannot safely replace a v0.2 MCP conversation in place. Existing active user
sessions remain on their installed version until an explicit migration is chosen.
