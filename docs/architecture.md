# Reach v0.3 architecture

Reach is a Rust process with independent MCP, file mutation, command-task and PTY
subsystems. The MCP host owns approval. Every HTTP request independently reloads
credentials and checks principal ownership; no MCP session ID supplies authority.

## One contract and one executor

`operation.rs` is the registry of operation names, Rust parameter/result types,
permissions, annotations, descriptions and handler/planner bindings. The registry
macro derives `Operation`, `Kind`, typed `Reply`, file-mutation dispatch, schema
discovery and typed execution dispatch. Add an argument type and handler/planner,
then register it once. There is no second handwritten signature list, permission
map, schema validator or native/compact execution implementation.

Serde decodes the same types Schemars inspects. Tagged sums distinguish create,
replace and append with different required fields. Bounded number/sequence types
share constants between decoding and schema generation. Filesystem, ownership,
capacity and process-state preconditions are runtime checks, not model hints.
The initialized guide resolves generated schema references, sums and optional
fields; detailed schema remains discoverable without repetition in every result.

Native mode exposes each permitted operation. Compact mode exposes Run with an
object argument and Discover; both dispatch the same typed Operation. The adapter
serializes Reply only when constructing MCP content. Image bytes become native
image blocks without duplication in structured metadata. Compact mode's single
Run annotation remains conservatively mutating. Reach cannot control host prompt
assembly, provider caching or whether a host uses initialization instructions.

`error::Failure` carries a stable code, message and recovery guidance. Invalid or
unauthorized calls return a rejected tool result. Accepted asynchronous operations
return durable task states, including typed execution errors and optional partial
outcomes. A failed Tasks/Poll observation is distinct from the original job failing.

## Durable tasks

`jobs` owns an explicit phase enum: queued, running, completed, failed, cancelled,
timed_out, interrupted, unknown, expired. Completion accepts a terminal-only type;
queued/running cannot accidentally be written as completed work. Typed task
results are command outputs or per-file receipts, never arbitrary JSON payloads.

Reservation is persisted before dispatch. Caller-generated operation_id plus a
fingerprint of owner, kind, normalized arguments and cwd recovers an identical
submission. Different arguments conflict; another principal cannot claim the ID.
A disconnected response never cancels the worker. Without an initial retry ID,
a lost submission is ambiguous and must be investigated through task discovery.
Restart marks all unfinished records interrupted; no commands or writes replay.

Notify subscriptions are registered before state/output inspection, avoiding lost
wakeups. Tasks waits for completion; Poll wakes for new bytes or completion.
Per-stream output is drained continuously even after its 16 MiB retention cap.
Text pages preserve complete trailing UTF-8 code points and expose decoding loss;
base64 pages are exact bytes. stdout/stderr remain distinct for command tasks.
The initial preview has source-byte continuation offsets, and Poll suppresses the
stored preview instead of repeating it.

Four workers serve up to 32 queued/running tasks, at most eight per principal.
File mutations serialize through a mutation gate; Shell can independently modify
files and does not participate in this gate. Completed payloads are bounded by
count, age, total log bytes and recovery bytes. Expiration persists a tombstone
before deletion. Fingerprints remain, so expiration never re-enables an old ID.
The finite 10,000-identity ledger refuses new work at capacity; moving to a new
namespace is an explicit operator action, not a hidden retry guarantee reset.
Periodic cleanup and startup cleanup complete interrupted deletions.

## File plans, receipts and recovery

Files and contextual patches produce `Change {path,before,after}` plans. A shared
commit engine validates sizes, encoding and target versions. Create requires an
absent target; replace, append, edit and patch existing sources require a SHA-256.
Exact edits reject ambiguous matches unless replacement of all is explicit.
Patch planning keeps BOMs and individual line endings, including mixed CRLF/LF.
Unsupported encodings are rejected instead of silently transcoded.

Preview never opens write handles. The result has global bounded diff output,
line-change summaries and before/proposed hashes. Actual writes hold Windows
non-sharing handles for all targets through validation, apply and rollback.
This blocks normal external open/write/rename operations in the critical region.
New paths reserve with create_new. Deletions mark disposition on the same locked
handle. Move destinations precede source deletions.

Existing files are updated through exclusive handles, **not crash-atomically
replaced**. Multi-file changes are not atomic transactions. Every file has its
own planned/applied/failed/not_attempted/rolled_back/rollback_failed outcome.
The failed write is included in rollback because it may have partially changed
bytes. An ordinary rollback never claims success when restoration failed.
Parent directories created during a plan's commit are not rolled back.

`journal` flushes all before-images and a manifest to the private state directory
before workspace writes. The final receipt is separately persisted. A crash can
leave any subset of proposed files and partial bytes; startup does not guess an
outcome or overwrite external work. Recover inspects the before/proposed/current
versions and recorded receipt. Explicit restore requires every current version,
rechecks versions under exclusive handles, creates a new durable task and saves
its own before-images. Recovery material expires with payload retention. This is
process-crash recovery material, not a power-loss atomicity or backup guarantee.

## Persistent terminals

`terminal` owns PTY resources with explicit auto-generated terminal_id. They share
credential identity, scopes and audit, not command-task state or HTTP sessions.
The shell keeps cwd/environment and can run interactive/fullscreen programs.
Close is distinct from confirmed process exit. Only actual OS wait results set
exit codes; shell prompt matching never determines command completion.

Windows `windows_pty` creates a ConPTY with a kill-on-close Job Object. The job
list is passed to CreateProcess while the main thread is suspended, then resumed;
user code never runs outside process-tree ownership. Exit/close kills remaining
descendants. Ordinary Shell uses a separate pipe model with a startup gate: user
commands are released only after its Job Object is attached. If Reach exits before
release, EOF prevents user command execution.

The bundled Microsoft ConPTY redistributable is loaded from an absolute path
beside Reach, never from workspace cwd. Its package/version/hash/signature checks
are in prepare-windows-runtime.ps1. Info reports whether bundled or system ConPTY
is available. The tested older system ConPTY flattens alternate-screen switches;
full terminal acceptance uses the bundled runtime. The DLL and OpenConsole host
remain external portable-package files with Microsoft's license notice.

Terminal input has a bounded queue, byte cap and per-session retry ledger. The
input fingerprint describes semantic input, so terminal mode changes do not alter
retry identity. `written` means OS-channel delivery, not application acceptance;
queued/unknown outcomes must not be blindly resent under a fresh ID. Close marks
unconfirmed input unknown and releases blocked writers by ending the process tree.
On restart old terminals are interrupted and output expires; no input is replayed.

`terminal_output` drains the PTY on its own thread. A 1 MiB byte ring offers exact
base64 reads with oldest/end/next cursors and explicit gap ranges. Eviction keeps
reading the producer; it does not block a TUI because the agent stopped polling.
VT parsing maintains Unicode cells, cursor, application-key modes, bracketed paste
and main/alternate screens. Screen snapshots and versions suppress unchanged
redraws; expected_screen makes pagination detect an intervening redraw. Text
snapshots are bounded; raw output is available when attributes/sequences matter.
Terminal queries receive bounded responses; OSC clipboard operations do not access
the user's clipboard. No claim is made of a graphical terminal's pixel rendering.

Default quotas: eight active terminals, four per principal; thirty minutes idle;
one-hour ended-output retention with at most 64 cached sessions. Metadata and
input retry ledgers are finite. Lifecycle locking prevents open/close races from
resurrecting a terminal. Reader errors terminate the child via its owner; console
destruction occurs on a different thread so draining cannot deadlock teardown.

## Authentication and deployment boundaries

256-bit OS-random Bearer secrets are hashed with SHA-256 and compared in constant
time. Scope/root/profile/expiry/revocation checks occur on each request. Principal
is supplied by credentials, never by operation arguments. Loopback Host checks,
Origin rejection and rate limits protect the private HTTP endpoint. This is not
an OAuth authorization server. Remote traffic uses SSH; direct public deployment
requires a separately designed TLS/OAuth integration.

The embedded management listener uses local one-use tickets, independent cookies,
CSRF, origin checks and CSP. Agent Bearer credentials cannot authorize management.
Audit contains operation/identity/outcome metadata, not arguments or file content.
Audit failures are visible on stderr; it is not a tamper-proof audit system.

File roots and resource ownership are API boundaries, not OS isolation. `exec`
provides the current account's capability and can access other data. Use separate
OS users/containers/VMs for untrusted agents. Windows is tested; Unix advisory locks
and process-group behavior are not claimed equivalent or production-validated.