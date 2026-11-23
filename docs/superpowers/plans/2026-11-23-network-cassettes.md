# Network Cassette Design Decisions

Design decisions for HTTP cassette recording and deterministic replay in Camoufox:

## Passive Recording
Passive page-session network events and a dedicated body-reading worker preserve request and cache behavior during recording. In-flight requests without a completed response at record stop are excluded. Unreadable or evicted response bodies are skipped and reported with their method, URL, and reason; the remaining index is still published. Cassette write errors are fatal and prevent index publication. Instance stop, shutdown, and close-tab have a total 15-second finalize deadline: at expiry the connection is closed to interrupt body reads, and affected exchanges are skipped with reason `finalize deadline exceeded`.

## Replay Validation
Cassettes are validated strictly before activating replay interception, including streamed SHA-256 and length validation for every referenced body. Bodies are read on demand and revalidated on every fulfillment. A missing or changed body aborts that request and makes replay stop report an `io error: ...`; modified data is never served. Index reads reject symlinks and non-regular files and are limited to 64 MiB.

## Redirect Refusal
Firefox Juggler does not retain redirect bodies and deliberately bypasses interception for redirected hops. Redirects are recorded with empty bodies, and replay explicitly refuses cassettes containing redirects prior to activation.

## Stop Ordering
Replay stop closes the event channel and drains pending accepted requests before disabling network interception, ensuring subsequent requests resume normal network behavior cleanly without races.
