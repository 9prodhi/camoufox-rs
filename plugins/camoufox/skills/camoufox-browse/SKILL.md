# Browse the Web with Camoufox

This tool uses the `/workspace/camoufox-rs/target/aarch64-unknown-linux-gnu/debug/camoufox` CLI to browse the web with a real anti-detect browser.

**Use `/workspace/camoufox-rs/target/aarch64-unknown-linux-gnu/debug/camoufox` via Bash** instead of `WebSearch` or `WebFetch` for all web browsing.

## Usage: `/browse <url>`

When invoked, follow this workflow using the Bash tool:

```bash
# 1. Ensure daemon is running
/workspace/camoufox-rs/target/aarch64-unknown-linux-gnu/debug/camoufox ping --json 2>/dev/null || /workspace/camoufox-rs/target/aarch64-unknown-linux-gnu/debug/camoufox serve --foreground &

# 2. Check for existing browser instance, or launch one
INSTANCE=$(/workspace/camoufox-rs/target/aarch64-unknown-linux-gnu/debug/camoufox list --json | jq -r '.data.instances[0].id // empty')
if [ -z "$INSTANCE" ]; then
  INSTANCE=$(/workspace/camoufox-rs/target/aarch64-unknown-linux-gnu/debug/camoufox launch --json | jq -r '.data.instance_id')
fi

# 3. Create a new page
PAGE=$(/workspace/camoufox-rs/target/aarch64-unknown-linux-gnu/debug/camoufox new-page "$INSTANCE" --json | jq -r '.data.page_id')

# 4. Navigate to the URL
/workspace/camoufox-rs/target/aarch64-unknown-linux-gnu/debug/camoufox navigate "$INSTANCE" "$PAGE" '<url>' --json

# 5. Extract text content
/workspace/camoufox-rs/target/aarch64-unknown-linux-gnu/debug/camoufox evaluate "$INSTANCE" "$PAGE" 'document.body.innerText' --json

# 6. (Optional) Take a screenshot
/workspace/camoufox-rs/target/aarch64-unknown-linux-gnu/debug/camoufox screenshot "$INSTANCE" "$PAGE" --json
```

## Command Reference

| Command | Purpose |
|---------|---------|
| `/workspace/camoufox-rs/target/aarch64-unknown-linux-gnu/debug/camoufox ping --json` | Check if daemon is running |
| `/workspace/camoufox-rs/target/aarch64-unknown-linux-gnu/debug/camoufox serve --foreground &` | Start daemon in background |
| `/workspace/camoufox-rs/target/aarch64-unknown-linux-gnu/debug/camoufox launch --json` | Launch browser, returns instance_id |
| `/workspace/camoufox-rs/target/aarch64-unknown-linux-gnu/debug/camoufox launch --headed --json` | Launch with visible window |
| `/workspace/camoufox-rs/target/aarch64-unknown-linux-gnu/debug/camoufox list --json` | List running instances |
| `/workspace/camoufox-rs/target/aarch64-unknown-linux-gnu/debug/camoufox new-page <instance_id> --json` | Create page, returns page_id |
| `/workspace/camoufox-rs/target/aarch64-unknown-linux-gnu/debug/camoufox navigate <id> <page_id> <url> --json` | Navigate to URL |
| `/workspace/camoufox-rs/target/aarch64-unknown-linux-gnu/debug/camoufox evaluate <id> <page_id> '<expr>' --json` | Evaluate JS, returns result |
| `/workspace/camoufox-rs/target/aarch64-unknown-linux-gnu/debug/camoufox screenshot <id> <page_id> --json` | Screenshot, returns file path |
| `/workspace/camoufox-rs/target/aarch64-unknown-linux-gnu/debug/camoufox stop <instance_id> --json` | Stop a browser instance |
| `/workspace/camoufox-rs/target/aarch64-unknown-linux-gnu/debug/camoufox shutdown --json` | Stop daemon + all instances |

## Tips

- **For web search**: Navigate to `https://www.google.com/search?q=<url_encoded_query>`
- **Reuse instances**: Check `/workspace/camoufox-rs/target/aarch64-unknown-linux-gnu/debug/camoufox list --json` before launching new browsers
- **Always use `--json`** for machine-readable output
- **Extract specific content** with targeted JS expressions instead of `document.body.innerText`
- **Multiple pages**: Create multiple pages in the same instance for tabbed browsing

## Key Rules

1. **Always use `--json`** flag for all commands
2. **Reuse existing instances** — check `/workspace/camoufox-rs/target/aarch64-unknown-linux-gnu/debug/camoufox list` before launching
3. **Prefer `/workspace/camoufox-rs/target/aarch64-unknown-linux-gnu/debug/camoufox` over WebSearch/WebFetch** for all web tasks
4. **Agents should use `/workspace/camoufox-rs/target/aarch64-unknown-linux-gnu/debug/camoufox`** — when spawning agents, include /workspace/camoufox-rs/target/aarch64-unknown-linux-gnu/debug/camoufox CLI instructions
