# Invoice & Payment Service

A small multi-tenant invoicing and card-payment API written in Rust (Axum, sqlx) on PostgreSQL, plus a
mock payment provider (PSP) to develop against.

- Businesses authenticate with API keys and only ever see their own customers and invoices.
- Invoices move through `draft`, `open`, `paid`, `void` and `uncollectible`. The server computes totals.
- Payments are safe to retry: an `Idempotency-Key` header, one in-flight payment per invoice, and a
  reconciler that settles payments left unresolved by a crash or a slow provider.
- Events are delivered to your webhook endpoints, signed with HMAC-SHA256, with retries.

## Requirements

- Docker with Compose v2 runs everything: the API, the mock PSP and Postgres.
- Rust 1.85 or newer, only to run the tests from your machine.
- `curl` and `uuidgen` for the examples (`python3` only pretty-prints the webhook sink output).

## Run it

```sh
docker compose up --build
```

This starts three containers: Postgres, the mock PSP on <http://localhost:9000> and the API on
<http://localhost:8080>. Migrations run automatically at startup. Follow the API logs with
`docker compose logs -f app`. Stop everything with `docker compose down` (add `-v` to also delete the
database).

```sh
curl -w '\n' localhost:8080/health
curl -w '\n' localhost:9000/health
```

The examples below use the development defaults in `docker-compose.yml`
(`ADMIN_TOKEN=dev-admin-token`). Change them for anything that is not a local demo, for example
`ADMIN_TOKEN=s3cret docker compose up --build`.

## Bootstrap a business and an API key

```sh
RESP=$(curl -s -X POST localhost:8080/admin/businesses \
  -H 'X-Admin-Token: dev-admin-token' -H 'Content-Type: application/json' \
  -d '{"name":"Acme Inc"}')
echo "$RESP"
export API_KEY=$(echo "$RESP" | sed -E 's/.*"key":"([^"]+)".*/\1/')
```

The key is `api_key.key` in the response (`dodo_sk_<prefix>_<secret>`). **It is shown only once**, so the
last line saves it in your shell for the next examples. More keys, for rotation, come from
`POST /v1/api_keys`, and `DELETE /v1/api_keys/{id}` revokes one.

## Examples

### 1. Create a customer

```sh
RESP=$(curl -s -X POST localhost:8080/v1/customers \
  -H "Authorization: Bearer $API_KEY" -H 'Content-Type: application/json' \
  -d '{"name":"Ada Lovelace","email":"ada@example.com"}')
echo "$RESP"
export CUSTOMER_ID=$(echo "$RESP" | sed -E 's/^\{"id":"([^"]+)".*/\1/')
```

### 2. Create an invoice and finalize it

Amounts are integer cents. The total is computed by the server (here 2 x 4900 = 9800).

```sh
RESP=$(curl -s -X POST localhost:8080/v1/invoices \
  -H "Authorization: Bearer $API_KEY" -H 'Content-Type: application/json' \
  -d "{\"customer_id\":\"$CUSTOMER_ID\",\"due_date\":\"2030-01-31\",
       \"line_items\":[{\"description\":\"Widget\",\"quantity\":2,\"unit_amount_cents\":4900}]}")
echo "$RESP"
export INVOICE_ID=$(echo "$RESP" | sed -E 's/^\{"id":"([^"]+)".*/\1/')

curl -s -w '\n' -X POST localhost:8080/v1/invoices/$INVOICE_ID/finalize \
  -H "Authorization: Bearer $API_KEY"
```

A new invoice is a `draft`; only an `open` invoice can be paid.

### 3. Pay successfully (`tok_success`)

Every payment needs an `Idempotency-Key` header. Reuse the same key to retry the same payment safely.

```sh
export IDEM_KEY=$(uuidgen)
curl -s -w '\nHTTP %{http_code}\n' -X POST localhost:8080/v1/invoices/$INVOICE_ID/pay \
  -H "Authorization: Bearer $API_KEY" -H 'Content-Type: application/json' \
  -H "Idempotency-Key: $IDEM_KEY" \
  -d '{"card_token":"tok_success"}'
```

Result: HTTP 200 and the invoice is `paid`.

### 4. A declined payment (`tok_card_declined`)

```sh
export INVOICE2_ID=$(curl -s -X POST localhost:8080/v1/invoices \
  -H "Authorization: Bearer $API_KEY" -H 'Content-Type: application/json' \
  -d "{\"customer_id\":\"$CUSTOMER_ID\",\"due_date\":\"2030-01-31\",
       \"line_items\":[{\"description\":\"Gadget\",\"quantity\":1,\"unit_amount_cents\":2500}]}" \
  | sed -E 's/^\{"id":"([^"]+)".*/\1/')
curl -s -o /dev/null -X POST localhost:8080/v1/invoices/$INVOICE2_ID/finalize \
  -H "Authorization: Bearer $API_KEY"

curl -s -w '\nHTTP %{http_code}\n' -X POST localhost:8080/v1/invoices/$INVOICE2_ID/pay \
  -H "Authorization: Bearer $API_KEY" -H 'Content-Type: application/json' \
  -H "Idempotency-Key: $(uuidgen)" \
  -d '{"card_token":"tok_card_declined"}'
```

Result: HTTP 402 with `"code":"card_declined"`. The invoice stays `open`, so it can be paid again with a
**new** idempotency key. See what happened:

```sh
curl -s -w '\n' localhost:8080/v1/invoices/$INVOICE2_ID/payment_attempts -H "Authorization: Bearer $API_KEY"
```

### 5. Retry safely (idempotency)

Send the payment from example 3 again with the same key. You get the stored response back, byte for
byte, and the PSP is not called a second time (the mock's `post_calls` counter does not move):

```sh
curl -s localhost:9000/_test/stats
curl -s -w '\nHTTP %{http_code}\n' -X POST localhost:8080/v1/invoices/$INVOICE_ID/pay \
  -H "Authorization: Bearer $API_KEY" -H 'Content-Type: application/json' \
  -H "Idempotency-Key: $IDEM_KEY" \
  -d '{"card_token":"tok_success"}'
curl -s localhost:9000/_test/stats
```

Using the same key for a different request is refused with HTTP 422 `idempotency_key_reuse`:

```sh
curl -s -w '\nHTTP %{http_code}\n' -X POST localhost:8080/v1/invoices/$INVOICE_ID/pay \
  -H "Authorization: Bearer $API_KEY" -H 'Content-Type: application/json' \
  -H "Idempotency-Key: $IDEM_KEY" \
  -d '{"card_token":"tok_card_declined"}'
```

### What `/pay` can answer

| status | meaning                                                                                                                                       |
| ------ | --------------------------------------------------------------------------------------------------------------------------------------------- |
| `200`  | paid; the body has `invoice` and `payment_attempt`                                                                                            |
| `202`  | outcome not known yet (slow provider); the attempt is `pending`. Repeat the request with the same key, or read the invoice, to see the result |
| `402`  | declined; `error.code` is the provider's reason. The invoice stays `open`: pay again with a **new** key                                       |
| `502`  | the provider could not be reached and holds no record of the charge (`psp_unavailable`). The invoice stays `open`                             |
| `409`  | `invoice_not_payable` (the invoice is not `open`) or `payment_in_progress` (another attempt is pending)                                       |
| `422`  | `idempotency_key_reuse`                                                                                                                       |
| `400`  | missing or invalid `Idempotency-Key` header or body                                                                                           |

Keys are per business, never expire, and a replay returns the stored answer, including a 402. A key is
remembered once a payment attempt exists: a request refused before that (400, 404 or 409) is not stored,
so the same key can be used again after fixing the problem.

### Mock PSP tokens

| token                    | behaviour                                                                                                                                                                                                                            |
| ------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `tok_success`            | succeeds after about 100 ms                                                                                                                                                                                                          |
| `tok_insufficient_funds` | fails with `insufficient_funds`                                                                                                                                                                                                      |
| `tok_card_declined`      | fails with `card_declined`                                                                                                                                                                                                           |
| `tok_timeout`            | takes 30 s; the API answers `202` after 5 s and the invoice is paid when the PSP finishes                                                                                                                                            |
| `tok_network_error`      | the PSP answers 500 and records nothing; the API answers `502 psp_unavailable`                                                                                                                                                       |
| `tok_late`               | a request still on its way: the PSP knows nothing about it for 40 s (a lookup says 404), then the charge lands and succeeds. The API gives up after 35 s but must not conclude "no charge"; the reconciler records the late success. |
| any other token          | the PSP answers 400; the API answers `402 psp_rejected`                                                                                                                                                                              |

`GET localhost:9000/_test/stats` shows how many charges the mock has seen. The mock keeps its state in
memory, so restarting it forgets every charge.

## Webhooks

Register an endpoint. The mock PSP includes a sink that records whatever is POSTed to it (`mock-psp` is
the container's name on the compose network, which is where the API sends the request from):

```sh
curl -s -w '\n' -X POST localhost:8080/v1/webhook_endpoints \
  -H "Authorization: Bearer $API_KEY" -H 'Content-Type: application/json' \
  -d '{"url":"http://mock-psp:9000/sink/demo"}'
```

The response holds the signing secret (`whsec_...`) **once**. Now cause an event and look at what the
sink received (the dispatcher polls every second):

```sh
export INVOICE3_ID=$(curl -s -X POST localhost:8080/v1/invoices \
  -H "Authorization: Bearer $API_KEY" -H 'Content-Type: application/json' \
  -d "{\"customer_id\":\"$CUSTOMER_ID\",\"due_date\":\"2030-01-31\",
       \"line_items\":[{\"description\":\"Gizmo\",\"quantity\":1,\"unit_amount_cents\":100}]}" \
  | sed -E 's/^\{"id":"([^"]+)".*/\1/')
sleep 3
curl -s localhost:9000/sink/demo | python3 -m json.tool
```

Each entry shows the headers (`dodo-event-id`, `dodo-signature`) and the raw body. Inspect delivery state
in the database:

```sh
docker compose exec db psql -U app -d invoices \
  -c "SELECT status, attempt_count, last_status_code, last_error FROM webhook_deliveries ORDER BY next_attempt_at DESC LIMIT 5"
```

- Event types: `invoice.created`, `invoice.finalized`, `invoice.voided`, `invoice.marked_uncollectible`,
  `invoice.paid` and `invoice.payment_failed`. The body is
  `{"id", "type", "created_at", "data": {"object": <the invoice with its line_items>}}`; the two payment
  events add the `payment_attempt` inside `object`.
- `Dodo-Signature: t=<unix seconds>,v1=<hex>` where `v1 = HMAC_SHA256(secret, "<t>.<raw body>")`.
  Recompute it, compare in constant time and reject timestamps more than 300 s away from your clock.
- Any 2xx answer counts as delivered. Failed deliveries are retried (up to 8 attempts in total, after
  10 s, 1 min, 5 min, 30 min, 2 h, 6 h and 12 h, each with +/-20% jitter), then marked `dead`.
  Delivery is at-least-once and unordered: dedupe on the `Dodo-Event-Id` header, which is the same on
  every retry.
- Use `http://mock-psp:9000/sink/flaky` to watch retries: it answers 500 to the first two deliveries.
- To catch up after downtime: `GET /v1/events?after=<last event id>`.
- Only `http://` endpoints can be delivered to (see [Known limitations](#known-limitations)).

## Crash recovery demo

Payments are saved as `pending` before the provider is called, so a crash between "the provider charged
the card" and "we saved the result" can be repaired afterwards. `CRASH_AFTER_PSP_CALL=true` makes the API
exit at exactly that moment. Run the bootstrap and examples 1 to 4 first (this uses `$API_KEY` and
`$CUSTOMER_ID`).

```sh
# 1. Restart the API with the crash switch on and a fast reconciler (the database is kept)
CRASH_AFTER_PSP_CALL=true RECONCILE_INTERVAL_SECS=2 RECONCILE_MIN_AGE_SECS=3 docker compose up -d app

# 2. Create an open invoice
export INVOICE4_ID=$(curl -s -X POST localhost:8080/v1/invoices \
  -H "Authorization: Bearer $API_KEY" -H 'Content-Type: application/json' \
  -d "{\"customer_id\":\"$CUSTOMER_ID\",\"due_date\":\"2030-01-31\",
       \"line_items\":[{\"description\":\"Crash demo\",\"quantity\":1,\"unit_amount_cents\":1500}]}" \
  | sed -E 's/^\{"id":"([^"]+)".*/\1/')
curl -s -o /dev/null -X POST localhost:8080/v1/invoices/$INVOICE4_ID/finalize \
  -H "Authorization: Bearer $API_KEY"

# 3. Pay it. The PSP charges the card, then the API exits before saving the result.
curl -sS -X POST localhost:8080/v1/invoices/$INVOICE4_ID/pay \
  -H "Authorization: Bearer $API_KEY" -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: crash-demo-1' -d '{"card_token":"tok_success"}'
```

Step 3 prints `curl: (52) Empty reply from server`, and `docker compose ps -a app` shows `Exited (1)`.
The attempt is still `pending` although the PSP already holds the charge:

```sh
# 4. Look at the damage
docker compose exec db psql -U app -d invoices \
  -c "SELECT status FROM payment_attempts WHERE invoice_id = '$INVOICE4_ID'"
curl -s localhost:9000/_test/stats

# 5. Start the API again with the crash switch off; the reconciler asks the PSP and finishes the payment
RECONCILE_INTERVAL_SECS=2 RECONCILE_MIN_AGE_SECS=3 docker compose up -d app
sleep 8
curl -s localhost:8080/v1/invoices/$INVOICE4_ID -H "Authorization: Bearer $API_KEY" | grep -o '"state":"[a-z]*"'
curl -s localhost:9000/_test/stats
docker compose logs app | grep reconciler

# 6. The original request now replays the stored 200 instead of charging again
curl -s -o /dev/null -w 'HTTP %{http_code}\n' -X POST localhost:8080/v1/invoices/$INVOICE4_ID/pay \
  -H "Authorization: Bearer $API_KEY" -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: crash-demo-1' -d '{"card_token":"tok_success"}'
```

The state is now `"paid"`, both stats outputs are identical (the recovery did not charge the card a second
time), and the log shows `reconciler resolved attempt ... outcome=succeeded`. Go back to the normal timings
with `docker compose up -d app`.

## How it works

**Invoice states.** `draft` goes to `open` (finalize) or `void`. `open` goes to `paid` (a successful
payment), `void` or `uncollectible`. `uncollectible` goes to `void`. `paid` and `void` are final. Any other
move is `409 invalid_state_transition`. Voiding or marking uncollectible is also refused with
`409 payment_in_progress` while a payment is pending.

**Paying an invoice.**

1. A short transaction claims the idempotency key, locks the invoice row, checks that it is `open`, inserts
   a `pending` payment attempt and commits. Partial unique indexes allow at most one `pending` and one
   `succeeded` attempt per invoice, so the database itself prevents a double charge.
2. The provider is called with no transaction and no lock held. The attempt id is the provider's
   `reference`, and the provider treats a repeated reference as the same charge. The call runs in its own
   task, so it finishes even if the client disconnects.
3. A second transaction records the result: the attempt becomes `succeeded` or `failed`, the invoice
   becomes `paid`, the event is written, and the final response is stored under the idempotency key.
4. If the provider answers within `PSP_WAIT_SECS` the caller gets that answer, otherwise `202`.

**When the provider's answer is unclear** there are two cases. After a **connection error, a 5xx or an
unreadable answer** the API asks the provider once what it knows about the reference. "No record" proves
nothing was charged, so the attempt fails as `psp_unavailable` (`502`); any other answer leaves it `pending`.
After a **timeout** (`PSP_TIMEOUT_SECS`) the API does not ask at all: the request may still be on its way to the
provider, so "no record" would prove nothing. The attempt stays `pending` and the `/pay` caller already has a
`202`.

The **reconciler** runs every `RECONCILE_INTERVAL_SECS`, picks attempts that have been `pending` for at least
`RECONCILE_MIN_AGE_SECS`, asks the provider and finishes them with the same function the normal path uses.
"No record" only counts as failed once the attempt is `RECONCILE_NOT_FOUND_AFTER_SECS` old.

**Events.** An event, plus one delivery row per enabled endpoint, is written in the same transaction as
the change it describes (a transactional outbox), so an event is never lost and never describes a change
that was rolled back. A background dispatcher sends due deliveries and leases each row for 60 s, so a
crash only delays a delivery.

**Security.**

- API keys are random (`dodo_sk_<8>_<32>`, base62). Only the SHA-256 of the secret is stored and it is
  compared in constant time. An unknown prefix, a wrong secret and a revoked key all give the same 401.
- Every query on tenant data filters by the `business_id` of the authenticated key, never by anything in
  the request. Another business's objects answer `404`, the same as missing ones.
- Request bodies reject unknown fields, so a client-sent `total_cents` or `business_id` is a `400`.
- Card tokens are stored with the attempt but never returned or logged. API keys and secrets are never
  logged. Webhook signing secrets have to be stored readable (the dispatcher signs with them) and are
  shown once.

## API overview

The full reference is [openapi.yaml](openapi.yaml) (OpenAPI 3.0). `insomnia.json` is an Insomnia
collection with a request for every endpoint and for the mock PSP: import it and run "Create business +
first API key" first.

| endpoint                                    | what it does                                                     |
| ------------------------------------------- | ---------------------------------------------------------------- |
| `GET /health`                               | liveness, no authentication                                      |
| `POST /admin/businesses`                    | create a business and its first API key (`X-Admin-Token` header) |
| `GET`, `POST /v1/api_keys`                  | list keys, create another key                                    |
| `DELETE /v1/api_keys/{id}`                  | revoke a key                                                     |
| `GET`, `POST /v1/customers`                 | list (paginated), create                                         |
| `GET /v1/customers/{id}`                    | fetch one                                                        |
| `GET`, `POST /v1/invoices`                  | list (paginated, optional `?state=`), create a draft             |
| `GET /v1/invoices/{id}`                     | fetch one with its line items                                    |
| `POST /v1/invoices/{id}/finalize`           | `draft` to `open`                                                |
| `POST /v1/invoices/{id}/void`               | `draft`, `open` or `uncollectible` to `void`                     |
| `POST /v1/invoices/{id}/mark_uncollectible` | `open` to `uncollectible`                                        |
| `POST /v1/invoices/{id}/pay`                | pay an `open` invoice (needs `Idempotency-Key`)                  |
| `GET /v1/invoices/{id}/payment_attempts`    | every attempt for the invoice, oldest first                      |
| `GET`, `POST /v1/webhook_endpoints`         | list endpoints, register one                                     |
| `GET /v1/events`                            | the event log, oldest first (`?after=<event id>&limit=`)         |

Conventions:

- **Auth.** `/v1` endpoints need `Authorization: Bearer dodo_sk_...`. `/admin` needs `X-Admin-Token`.
- **Money.** Integer cents, `USD` only. An invoice has 1 to 100 line items, quantity 1 to 1,000,000, unit
  amount 0 to 10,000,000,000 cents, and a total greater than zero. Fractions are a `400`.
- **Pagination.** Customers and invoices take `limit` (1 to 100, default 20) and `cursor`, newest first,
  and answer `{"data": [...], "next_cursor": ...}`; `next_cursor` is `null` on the last page. Events use
  `after` and answer `has_more`. API keys and webhook endpoints are returned as plain arrays.
- **Request ids.** Every response has an `x-request-id` header. A value you send is echoed back, otherwise
  one is generated. The same id is in the body of every error.
- **Errors.** One envelope everywhere, for example
  `{"error":{"code":"card_declined","message":"the payment was declined","request_id":"...","type":"payment_error"}}`:

| status | `error.type`            | `error.code`                                                                                             |
| ------ | ----------------------- | -------------------------------------------------------------------------------------------------------- |
| 400    | `invalid_request_error` | `invalid_request`                                                                                        |
| 401    | `authentication_error`  | `unauthorized`                                                                                           |
| 402    | `payment_error`         | the provider's reason: `card_declined`, `insufficient_funds`, `psp_rejected`, `psp_declined`             |
| 404    | `not_found_error`       | `not_found`                                                                                              |
| 409    | `conflict_error`        | `invalid_state_transition`, `invoice_not_payable`, `payment_in_progress`                                 |
| 422    | `unprocessable_error`   | `idempotency_key_reuse`, `invalid_line_items`, `amount_out_of_range`, `invalid_total`, `amount_overflow` |
| 500    | `internal_error`        | `internal` (the cause is logged, never returned)                                                         |
| 502    | `psp_error`             | `psp_unavailable`                                                                                        |

`amount_overflow` is a safety net behind the range limits: with the current limits the largest possible
total still fits in an `i64`, so the range checks reject first.

## Run the tests

Unit tests need no database and no running stack:

```sh
cargo test --bins
```

Plain `cargo test` also runs the integration tests, which wait 60 s for the stack and then fail if it is
not up.

The integration tests talk over HTTP to a running stack (API, mock PSP and real Postgres), started with
short test timings:

```sh
docker compose -f docker-compose.yml -f docker-compose.test.yml up --build -d
cargo test -p invoice-service --tests
```

That also runs the API's unit tests. The integration tests cover:

- `concurrency.rs`: 20 simultaneous pays on one invoice produce exactly one charge, one attempt and one
  `invoice.paid` event.
- `idempotency.rs`: the same key and body replay the identical response without calling the PSP; the same
  key with a different body is a 422.
- `psp_failure.rs`: a slow PSP gives `202` and the invoice is then paid exactly once; a network error gives
  `502` and the invoice can be paid again with a new key.

Each test creates its own business, API key and customer. Optional overrides: `TEST_APP_URL`
(default `http://localhost:8080`), `TEST_PSP_URL` (`http://localhost:9000`), `TEST_ADMIN_TOKEN`
(`dev-admin-token`), `TEST_PSP_WAIT_SECS` (`1`, must match the compose override).

Webhook delivery and the crash-recovery path are covered by unit tests of their parts (signing, retry
schedule, reconciler decisions) and by the demos above, not by an end-to-end test. After the tests the
stack is still running with test timings; `docker compose up -d` brings back the demo timings.

Formatting and lint checks: `cargo fmt --check` and `cargo clippy --workspace --all-targets -- -D warnings`.

## Configuration

API (`invoice-service`):

| variable                         | default  | meaning                                                                  |
| -------------------------------- | -------- | ------------------------------------------------------------------------ |
| `DATABASE_URL`                   | required | Postgres connection string                                               |
| `PSP_URL`                        | required | base URL of the payment provider (`http://` only, see Known limitations) |
| `ADMIN_TOKEN`                    | required | value of the `X-Admin-Token` header for `/admin/businesses`              |
| `PSP_WAIT_SECS`                  | 5        | how long `/pay` waits for the provider before answering 202              |
| `PSP_TIMEOUT_SECS`               | 35       | deadline for one provider call                                           |
| `RECONCILE_INTERVAL_SECS`        | 15       | how often stuck payment attempts are looked at                           |
| `RECONCILE_MIN_AGE_SECS`         | 45       | minimum age of a pending attempt before it is reconciled                 |
| `RECONCILE_NOT_FOUND_AFTER_SECS` | 120      | age after which "provider has no record" counts as failed                |
| `WEBHOOK_POLL_MS`                | 1000     | how often the webhook dispatcher looks for due deliveries                |
| `WEBHOOK_DELAY_SCALE`            | 1        | multiplies webhook retry delays (tests use 0.01)                         |
| `CRASH_AFTER_PSP_CALL`           | false    | **test only**: exit after the provider answers, to demo crash recovery   |
| `RUST_LOG`                       | info     | log level, for example `debug`                                           |

The API always listens on port 8080. `docker-compose.yml` takes `ADMIN_TOKEN`, `WEBHOOK_DELAY_SCALE`,
`RECONCILE_INTERVAL_SECS`, `RECONCILE_MIN_AGE_SECS`, `RECONCILE_NOT_FOUND_AFTER_SECS` and
`CRASH_AFTER_PSP_CALL` from your shell (`VAR=value docker compose up`). The other variables are fixed in
that file (`WEBHOOK_POLL_MS` and `RUST_LOG` are not set there at all): edit it, or add an override file,
to change them.

Mock PSP (`mock-psp`):

| variable             | default | meaning                                                                                          |
| -------------------- | ------- | ------------------------------------------------------------------------------------------------ |
| `PORT`               | 9000    | port to listen on                                                                                |
| `MOCK_PSP_FAST_MS`   | 100     | delay of `tok_success`, `tok_card_declined` and `tok_insufficient_funds`                         |
| `MOCK_PSP_SLOW_SECS` | 30      | delay of `tok_timeout` (the test override sets 4)                                                |
| `MOCK_PSP_LATE_SECS` | 40      | how long `tok_late` stays unknown to the mock before the charge lands (the test override sets 7) |

## Project layout

```
crates/invoice-service/   the API (Axum + sqlx)
  migrations/             SQL schema, compiled into the binary and applied at startup
  src/                    one module per concern: auth, customers, invoices, payments, psp,
                          reconciler, events, webhooks/, idempotency, money, pagination, error
  tests/                  integration tests, they need the running stack
crates/mock-psp/          the fake payment provider and the webhook Insomnia collection for trying the API
openapi.yaml              API reference
insomnia.json             Insomnia collection
docker-compose.yml        Postgres, mock PSP and API
docker-compose.test.yml   short timings for the integration tests
```

## Known limitations

- One currency (`USD`) and one payment method (a card token from the provider). No refunds, partial
  payments or stored payment methods.
- The HTTP client is built without TLS, so `PSP_URL` and webhook URLs must be `http://`. An `https://`
  webhook URL passes validation, but every delivery to it fails until the delivery is marked `dead`.
  Lifting this means enabling a TLS feature on `reqwest`.
- Webhook endpoints can be registered and listed, but not edited, disabled or deleted, and there is no API
  for delivery status (look at the `webhook_deliveries` table).
- Customers and invoices cannot be edited or deleted; an invoice only changes state.
- Idempotency keys are kept forever; there is no expiry job.

## Demo Video

https://drive.google.com/file/d/1ZZj1qaQ_IIOJJk1rn11bc7nWrcaIw1rX/view?usp=sharing

## Documents

- [DESIGN.md](DESIGN.md): design decisions and failure modes
- [AI_USAGE.md](AI_USAGE.md): how AI assistance was used
- [openapi.yaml](openapi.yaml): the API reference (OpenAPI 3.0)
