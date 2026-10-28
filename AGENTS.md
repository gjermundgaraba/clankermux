# Agent conventions

Code carries no comments except legal headers, foreign/platform gotchas we cannot change, public-API docs, issue/RFC links, and lint-required directives; express intent through names and types.

Set up with `mise install` (zig and just from `mise.toml`), then `cd web && pnpm install && pnpm -C apps/client browsers` (Playwright's Chromium for the browser tests), then `just check`.
