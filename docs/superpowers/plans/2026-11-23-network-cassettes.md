# Network cassette implementation plan

Goal: implement the supplied record/replay contract as a CLI-only testing feature.

Architecture: passive page-session events feed a worker that reads decoded response bodies. Replay validates and preloads a cassette before enabling page interception. A managed page owns the worker and subscription; dropping it unsubscribes and joins the worker.

Constraints: remain in this worktree, no commits or version changes, no plugin changes, default library dependencies unchanged, use the command and cassette names from the supplied contract.

- [x] Add cassette serialization, scrubbing, strict validation, body deduplication, and occurrence matching with unit tests.
- [x] Add page-owned worker lifecycle and event handling; wire commands through clap, IPC, daemon, manager, and output.
- [x] Add ignored local-server browser tests for offline replay, misses, filters, body matching, invalid arguments, and cleanup. Document commands and cassette behavior.
- [x] Run formatting, clippy, full tests, rustdoc, and the added browser tests with an isolated runtime directory; inspect the final diff.

Review focus: binary request bodies, response-body eviction/errors, redirect ordering, stop while work is queued, and page/instance closure. In-flight requests without a completed response at record stop are excluded. Body read/write errors fail record stop instead of silently producing an incomplete cassette.


Implementation decisions:
- Passive page-session network events and a dedicated body-reading worker preserve request and cache behavior during recording.
- Validated replay bodies are preloaded in memory, preventing disk changes from changing active replay responses.
- Juggler does not retain redirect bodies and deliberately bypasses interception for redirected hops. Redirects are recorded with empty bodies; replay rejects redirect cassettes before activation. Full redirect replay remains unsupported by the existing page API.
- Replay stop closes the event sender, drains accepted requests, then disables interception. Later requests resume normal network behavior. An independent review identified the unsafe reverse ordering; the batch-request regression reproduced it and passed after the fix.

Validation:
- cargo fmt --check: passed.
- cargo clippy --all-targets --all-features -- -D warnings: passed.
- cargo test --all-features: passed (405 library tests, 7 binary tests, 21 doctests; 80 browser tests ignored by default).
- RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features: passed.
- cargo test --all-features --test network_cassette -- --ignored --test-threads=1: passed (6 browser tests). Browser auto-detected with CAMOUFOX_BIN unset; XDG_RUNTIME_DIR isolated under this worktree before daemon/browser runs.
- cargo tree --no-default-features -e normal: confirmed default library dependencies are unchanged.

Read-only independent review: one important stop-order finding fixed with a failing-then-passing browser regression; no deferred findings. No commits or branch changes performed.
