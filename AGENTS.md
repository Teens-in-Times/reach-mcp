# Reach

Standalone Rust MCP server. Do not depend on another executor, agent, or Python runtime.

The calling client owns user approval. This process validates authentication, authorization, argument contracts and task ownership, then executes accepted operations.

`auth` owns credential identity and revocation; `catalog` owns tool contracts and the initialization guide; `engine` owns operation dispatch; `http` owns MCP/HTTP; `jobs` owns durable task identity and output; `patch`, `files`, `image`, `process` implement operations.

Run `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings` and `cargo test --locked`. Auth/ownership changes require HTTP tests, not only helper tests. Keep secrets, private config, task logs, generated binaries and state out of Git and release archives. Never mutate a running deployment while testing; use a separate listener and private state.

Windows is the verified execution target. Pipe stdin is not a PTY. Filesystem access uses the current OS account; task ownership is not an OS sandbox. Do not claim model prompt caching is controlled by this server.
