# AI Usage

**The system is mine, and the prompts were guided by me.** I designed the architecture: the data model, the invoice state machine, the payment flow and how it fails, the webhook design, the API-key model, and the list of things I chose to cut. That design (sections 1 to 7 of `DESIGN.md`) was written before any code was generated, and every brief refers to it as `DESIGN.md`. I updated it afterwards to match what was built. The one part of the file that Claude drafted is section 8 (tenancy and auth beyond API keys, designed but not built); see section 1 below. Claude Code wrote most of the code, and I directed it with prompts built on that design: a rules file (`CLAUDE.md`) fixed the stack and the hard rules, and each step had a written brief that said what to build, which part of `DESIGN.md` to follow, and which "Done when" checks had to be run and shown. `AI_LOG.md` records each step: what was generated, which extra dependency the AI asked to add, and what I corrected.

## 1. AI tools and what I used them for

The AI tool was Claude Code (Anthropic's terminal coding agent). It ran Claude Sonnet 5.5 for nearly all turns and Claude Fable 5.1 for the first few.

- **Implementation, one step at a time.** Claude wrote the Rust from my briefs: workspace, config and the single JSON error envelope; embedded sqlx migrations (my SQL schema, with the instruction to change nothing and to tell me if something looked wrong); API-key auth, rotation and revocation; customers with keyset pagination; invoice totals with checked `i64` arithmetic and the state machine; the mock PSP and its webhook sink; the signed-webhook outbox dispatcher; the idempotent pay flow; the reconciler; the integration tests; the README and `openapi.yaml`.
- **Tests I specified, Claude wrote.** For example, 20 simultaneous pays with 20 different idempotency keys must give exactly one 200 and exactly one charge at the PSP, and the same key with a different body must give 422. They run against real Postgres, with no database mocks.
- **A comparison of my design with an existing production payments codebase.** I pointed Claude at it to read, strictly as a reference for domain behaviour, and told it not to copy code, names, schemas or comments. It wrote a private note, kept out of this repository, listing gaps it saw in my design. Claude suggested; I decided what to act on.
- **An adversarial review (Step 12).** I asked Claude to act as a sceptical senior payments engineer, read `DESIGN.md` and the code, and try to break it. It reported 17 findings with evidence and ran nine small experiments against the running stack. The full write-up is `docs/REVIEW.md`.
- **A design write-up for something I chose not to build (section 8 of `DESIGN.md`).** The assignment lists "OAuth or any auth beyond API keys" as out of scope and asks for it to be written about instead of built. I picked that topic and the starting idea: one realm per merchant, with that merchant's apps and customers inside it. I pointed Claude at an existing codebase as a reference and asked for a simple system design. Claude drafted section 8 and made small edits in sections 6 and 7 so that they point to it.
- **Learning Rust.** I come from JavaScript and TypeScript, so I had Claude explain the code by comparison with Node (`docs/RUST_FOR_NODE_DEVS.md`) and write an endpoint-by-endpoint flow guide (`docs/USER_FLOW.md`).
- **Small helpers.** An Insomnia import file for every endpoint (`insomnia.json`), a README check where Claude read the code first, without the docs, and then compared the README against it, and the first draft of this file, written from `AI_LOG.md` and my session records.

Apart from section 8 and the links to it in sections 6 and 7, `DESIGN.md` is my own writing. The AI also did not set the rules in `CLAUDE.md` or decide the schema.

## 2. Three decisions I made myself

### Decision 1: Postgres does the coordinating, with no queue, no cache and no lock held across the PSP call

- **What the AI proposed:** nothing on the mechanism. The stack was fixed in `CLAUDE.md` before any code, and the payment brief said to implement `DESIGN.md` section 3 exactly. The one change the AI made is the lock order when a payment attempt is finalised (Step 8). My text updated the attempt first and the invoice second, the opposite of the pay step, and Claude pointed out that two requests could then wait on each other and deadlock. I accepted the fix and wrote the order into `DESIGN.md`.
- **What I chose:** a short `SELECT ... FOR UPDATE` on the invoice to create a pending attempt, the PSP call outside any transaction, a second short transaction to finalise, and two partial unique indexes (one pending and one succeeded attempt per invoice) as a backstop. `SKIP LOCKED` lets the reconciler and the webhook dispatcher pick up work without a queue.
- **Why:** advisory locks add a concept for the same effect, SERIALIZABLE needs retry loops, and optimistic versioning needs retries and still needs the unique index. A lock held for the 30 s PSP call would stall every request touching that row and exhaust the pool. Each extra component is another failure mode.
- **Check:** the test `twenty_concurrent_pays_charge_exactly_once` covers it, and the review's own experiments (20 pays with different keys, 10 with the same key) each produced exactly one charge at the PSP.

### Decision 2: API keys are stored as a SHA-256 of a random secret, not argon2 or bcrypt

- **What the AI proposed:** nothing. I wrote "compare sha256(secret) with key_hash using constant-time comparison" into the Step 3 brief and Claude built that.
- **What I chose:** a key is `<prefix>_<secret>`. The prefix is stored in clear as a lookup handle, only the SHA-256 of the secret is stored, and the full key is shown once. Comparison is constant-time, and an unknown prefix, a wrong secret and a revoked key all return the same 401.
- **Why:** the secret is long and random (about 190 bits), so brute force is not realistic, while a slow password hash would add latency to every request and give an attacker a way to burn CPU.

### Decision 3: fix the review finding that risked a double charge, and write the other gaps down instead of building the AI's fixes

- **What the AI proposed:** the review rated F2 (a charge that lands after the attempt was marked failed is never noticed) and F3 (a pending attempt can stay pending forever and blocks the invoice) as High, and gave fix options for nearly every finding: re-check failed attempts for a time window, a new "unresolved" state with an admin route, a per-business event sequence, purging idempotency keys, and more.
- **What I chose:** I had F1 fixed (section 3). F2, F3, an events feed that can skip an event, unbounded list endpoints, idempotency keys that are never purged, and create endpoints with no idempotency key are listed in `DESIGN.md` section 7 under "Known gaps I found by attacking my own code and did not fix".
- **Why:** F1 was the code contradicting a rule in my own design, on the money path. The others are gaps in the design itself, and their real fixes (a PSP-side void for F2, a new attempt state for F3, a per-business sequence for events) change the schema or need a real PSP. I did not want to half-build them in code that moves money, so each one is stated with its production fix.

## 3. One thing I had to correct: a PSP timeout was judged "no charge" (review finding F1)

- **What was wrong:** the first pay path treated every unclear PSP answer the same, including its own 35 s timeout. It made one immediate lookup, and a "not found" failed the attempt as `psp_unavailable`, returned 502 and left the invoice payable with a new key. A slow request can still land at the PSP afterwards, so a client retry could charge the customer twice. `DESIGN.md` says a timeout gets no verdict until the reconciler's 2 minute rule, so the code had two policies.
- **Where it came from:** my Step 8 brief listed "timeout" together with "connection error" and "5xx" as one Ambiguous class and asked for one immediate lookup, which contradicts `DESIGN.md`. Claude built the brief as written and did not notice the conflict, so the rule in `CLAUDE.md` ("if code and DESIGN.md disagree, stop and ask me") never triggered. It was partly my brief and partly the AI's silence.
- **How it was found:** the Step 12 review found it by reading. It could not be reproduced against the mock PSP, which records a charge the moment it receives it.
- **How it was fixed:** the fix went into the code, not the design, because `DESIGN.md` already had the right rule and relaxing it would remove the protection against paying twice. The tests came first. The mock PSP got a `tok_late` token (it records nothing for a while, then the charge lands), and an integration test failed on the old code (the attempt was failed as `psp_unavailable` exactly 5 s in, and the charge landed at the PSP 2 s later) and passes on the new code (the attempt ends `succeeded`, with one charge). A timed-out attempt now stays `pending` for the reconciler; connection errors and 5xx keep the immediate lookup. `DESIGN.md` section 3 ("Ambiguous PSP answers") states which case gets which treatment.

### How I checked the rest

- Each step ended with `cargo fmt`, `cargo clippy -- -D warnings` and `cargo build`, then the brief's "Done when" checks, run and shown to me.
- The integration suite was run five times in a row against the compose stack (`AI_LOG.md`, Step 10).
- Every curl example in the README was run against the live stack, and `openapi.yaml` was linted and checked against 41 live responses (`AI_LOG.md`, Step 11).
- The adversarial review is the check that found the bug above, because it compares the code with my design instead of trusting the code.
