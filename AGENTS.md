# Reach

Standalone Rust MCP server. Do not depend on another executor, agent, or Python runtime.

The calling client owns user approval. This process validates authentication, authorization, argument contracts and task ownership, then executes accepted operations.

`auth` owns credential identity and revocation; `operation` owns the single operation registry, argument/result contracts and generated dispatch; `catalog` derives discovery and the initialization guide from those types; `engine` owns typed handlers and scheduling; `http` owns MCP/HTTP; `jobs` owns durable command/mutation phases and bounded logs; `files` and `patch` plan changes; `changes` owns exclusive commits and per-file receipts; `journal` owns durable before-images and guarded recovery; `terminal` owns persistent PTYs, `terminal_output` owns bounded streams and VT screens, and `windows_pty` owns native Windows process/ConPTY handles.

Run `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings` and `cargo test --locked`. Auth/ownership changes require HTTP tests, not only helper tests. Keep secrets, private config, task logs, generated binaries and state out of Git and release archives. Never mutate a running deployment while testing; use a separate listener and private state.

Windows is the primary execution target. Prepare Microsoft's pinned ConPTY runtime with `scripts/prepare-windows-runtime.ps1` beside test executables (target/debug/deps) before the full PTY suite. Pipe stdin is not a PTY. Filesystem access uses the current OS account; task ownership is not an OS sandbox. Do not claim model prompt caching is controlled by this server. Track remaining v0.3 acceptance work in docs/refactor-v0.3.md; passing the current tests alone is not completion of the full refactor.
