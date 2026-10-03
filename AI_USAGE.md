# AI Usage

## Tool

Claude Code (Anthropic's terminal coding agent), running Claude Sonnet 5.5 for nearly all turns and Claude Fable 5.1 for the first few.

## Who did what

| I did | Claude did |
| ----- | ---------- |
| Wrote the design in `DESIGN.md` (sections 1 to 7) before any code: data model, invoice states, payment flow and failure modes, webhooks, API keys, what to cut | Wrote most of the Rust, one step at a time, from my written briefs |
| Chose the stack and hard rules, and the "Done when" checks for each step | Wrote the tests I specified, the README and `openapi.yaml` |
| Decided which review findings to fix and which to document | Reviewed my design and code as a sceptical payments engineer (17 findings) |
| Chose the topic for section 8 (tenancy and auth beyond API keys, designed but not built) | Drafted section 8 of `DESIGN.md` |

Section 8 is the only part of `DESIGN.md` that Claude drafted. The schema and the rules were mine.

## Decisions I made

1. **Postgres does the coordinating.** A short row lock creates a pending attempt, the PSP is called with no lock held, and unique indexes stop a double charge. No queue, no cache. Claude proposed nothing here, but it did catch one deadlock risk in my lock order, and I accepted that fix.
2. **API keys are stored as a SHA-256 of a random secret, not argon2.** The secret is long and random, so a slow hash only adds latency to every request.
3. **Fix the bug that could double-charge, document the rest.** I fixed the finding on the money path. The other gaps are listed in `DESIGN.md` section 7 with their production fixes, because they need a schema change or a real PSP.

## One bug the AI missed, and how it was fixed

- **Bug:** a PSP timeout was treated as "no charge". The API then returned 502 and allowed a retry, so a slow request landing later could charge the customer twice.
- **Cause:** my brief grouped timeouts with connection errors, which contradicted my own design. Claude built the brief as written and did not flag the conflict.
- **Fix:** the code, not the design. A timed-out attempt now stays `pending` for the reconciler. I wrote the failing test first (a `tok_late` mock token whose charge lands after the timeout), then confirmed it passes.

## How I checked the work

- Each step ran `cargo fmt`, `cargo clippy -- -D warnings` and `cargo build`, then the brief's checks, shown to me.
- The integration tests run against real Postgres with no database mocks. For example, 20 simultaneous pays must produce exactly one charge.
- Every README example was run against the live stack.
