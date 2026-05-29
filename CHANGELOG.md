# Changelog

All notable changes to camoufox-rs are documented here.

## [Unreleased]

### Added

- **G1 — `cookies <instance_id>` command**: exports the full in-session cookie jar via the
  Juggler `Network.getCookies` call. Includes `HttpOnly` cookies (which are inaccessible from
  JavaScript). `--json` returns full cookie objects (name, value, domain, path, httpOnly,
  secure, sameSite, expires, session, size). The exported jar can be passed directly to
  host-side HTTP clients (e.g. `curl --cookie`) to fetch session-gated content without
  requiring in-session XHR round-trips.

- **G3 — `navigate --wait-until load|domcontentloaded`**: the `navigate` command now accepts
  a `--wait-until` flag that blocks until the specified browser lifecycle event fires, bounded
  by `--timeout` seconds. Accepted values: `load` (fires after all resources on the page have
  loaded) and `domcontentloaded` (fires once the HTML is parsed, before sub-resources). Any
  other value is rejected with an error at dispatch time.

- **G4 — `navigate` reports `status_code`**: `--json` output now includes `status_code`, the
  final main-document HTTP status code after following all redirects. `status_code` is `null`
  when the status is uncapturable (e.g. `about:` pages, navigation that errors before a
  network response is received). Navigate never fails on 4xx/5xx responses — the result is
  always `"ok": true` and the caller inspects `status_code` to decide how to proceed.

### Motivation

These three gaps (G1, G3, G4) were identified during the legal-agent eCourts spike:
`docs/superpowers/findings/2026-05-29-ecourts-spike-findings.md` (in the `legal-agent` repo).
The eCourts portal gates case documents behind session cookies set by authenticated page
navigations; extracting those cookies and replaying them host-side (G1) eliminates the need
for in-browser XHR workarounds and unblocks robust, resumable corpus fetching. Deterministic
load-event waiting (G3) and redirect-aware status inspection (G4) provide the scaffolding
needed to drive the multi-step login→search→download flow reliably.
