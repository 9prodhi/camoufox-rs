# Network Cassette Design Decisions

Design decisions for HTTP cassette recording and deterministic replay in Camoufox:

## Passive Recording
Passive page-session network events and a dedicated body-reading worker preserve request and cache behavior during recording. In-flight requests without a completed response at record stop are excluded, and body read or write errors fail record stop instead of silently producing an incomplete cassette.

## Replay Validation
Cassettes are validated strictly and bodies are preloaded in memory before activating replay interception, preventing concurrent disk modifications from altering active replay responses.

## Redirect Refusal
Firefox Juggler does not retain redirect bodies and deliberately bypasses interception for redirected hops. Redirects are recorded with empty bodies, and replay explicitly refuses cassettes containing redirects prior to activation.

## Stop Ordering
Replay stop closes the event channel and drains pending accepted requests before disabling network interception, ensuring subsequent requests resume normal network behavior cleanly without races.
