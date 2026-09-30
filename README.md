# Invoice & Payment Service

A small invoice and payment backend in Rust (Axum, SQLx, Postgres). Businesses create customers and invoices, customers pay invoices through a payment processor, and businesses are notified of every state change via signed webhooks.

- **[DESIGN.md](DESIGN.md)**: data model, state machine, payment failure modes, webhooks, API keys, and what was cut.
- **[docs/API.md](docs/API.md)**: endpoints, request and response shapes, error format, webhook verification.
- **[AI_USAGE.md](AI_USAGE.md)**: how AI tools were used.

## Demo Video

> **TODO:** add the Loom / Drive link here before submitting.

## Running it

Requirements: Docker with Compose v2.

```bash
docker compose up --build
```

This starts four containers:

| Service | Port | What it is |
|---|---|---|
| `api` | 8080 | The invoice service. Runs migrations on startup, plus the webhook dispatcher and payment reconciler |
| `db` | 5433 | Postgres 17 (5433 on the host to avoid clashing with a local Postgres) |
| `mock-psp` | 9090 | The mock payment processor, whose behaviour is picked by card token |
| `webhook-sink` | 9100 | Logs every webhook it receives. Paths starting with `/fail` answer 500 |

Businesses are onboarded by an operator, not over the API. Create one and keep the key it prints (it is shown only once):

```bash
docker compose exec api invoice-service create-business "Acme Inc"
```

```bash
export API=http://localhost:8080
export KEY=sk_...   # from the command above
```

## Walkthrough with curl

**0. Register a webhook endpoint** (optional). This points at the bundled sink; watch it with `docker compose logs -f webhook-sink`.

```bash
curl -s -X POST $API/v1/webhook_endpoints \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -d '{"url": "http://webhook-sink:9100/acme"}'
```

**1. Create a customer**

```bash
curl -s -X POST $API/v1/customers \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -d '{"name": "Ada Lovelace", "email": "ada@example.com"}'
```

```bash
export CUSTOMER_ID=...   # "id" from the response
```

**2. Create an invoice, then finalize it.** The server computes the total, here 49900 + 3 × 1500 = 54400 cents. Invoices start as `draft` and must be finalized (`open`) before they can be paid.

```bash
curl -s -X POST $API/v1/invoices \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -d '{
        "customer_id": "'$CUSTOMER_ID'",
        "due_date": "2026-12-31",
        "line_items": [
          {"description": "Pro plan (annual)", "quantity": 1, "unit_amount_cents": 49900},
          {"description": "Extra seats",       "quantity": 3, "unit_amount_cents": 1500}
        ]
      }'
```

```bash
export INVOICE_ID=...   # "id" from the response
```

```bash
curl -s -X POST $API/v1/invoices/$INVOICE_ID/finalize -H "Authorization: Bearer $KEY"
```

**3. A failed payment.** This returns `402`, the attempt is recorded as failed, the invoice stays `open`, and `invoice.payment_failed` is sent.

```bash
curl -s -i -X POST $API/v1/invoices/$INVOICE_ID/pay \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -H "Idempotency-Key: pay-$INVOICE_ID-1" \
  -d '{"card_token": "tok_card_declined"}'
```

**4. A successful payment.** This returns `200`, the invoice becomes `paid`, and `invoice.paid` is sent. Run it twice: the second response is identical, carries `Idempotent-Replayed: true`, and no second charge happens.

```bash
curl -s -i -X POST $API/v1/invoices/$INVOICE_ID/pay \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -H "Idempotency-Key: pay-$INVOICE_ID-2" \
  -d '{"card_token": "tok_success"}'
```

**5. Inspect the results**

```bash
curl -s $API/v1/invoices/$INVOICE_ID/payment_attempts -H "Authorization: Bearer $KEY"
```

```bash
curl -s "$API/v1/events?limit=10" -H "Authorization: Bearer $KEY"
```

### Card tokens

| Token | What the mock PSP does | What the service does |
|---|---|---|
| `tok_success` | Succeeds after ~100 ms | `200`, invoice `paid` |
| `tok_insufficient_funds` | Fails (`insufficient_funds`) | `402`, invoice stays `open` |
| `tok_card_declined` | Fails (`card_declined`) | `402`, invoice stays `open` |
| `tok_timeout` | Sleeps 30 s, then succeeds | `202` with a `pending` attempt after 5 s. The reconciler picks up the success ~45 s after the request, and the invoice becomes `paid` with one charge |
| `tok_network_error` | Returns 500 and charges nothing | `202` `pending`. After ~19 min of re-checks the attempt is closed as `failed/psp_unavailable` and the invoice is payable again |

To watch the timeout path, pay a fresh invoice with `tok_timeout` and follow `docker compose logs -f api`.

## Tests

The tests that matter most are integration tests against a real Postgres and a real (in-process) mock PSP:

| Test | Asserts |
|---|---|
| `payment_concurrency::concurrent_payments_charge_the_invoice_exactly_once` | 20 concurrent `POST /pay` calls with different keys: exactly one succeeds, the PSP sees one request, and there is one attempt and one `invoice.paid` |
| `payment_concurrency::concurrent_retries_with_one_key_share_a_single_attempt` | 20 concurrent calls with the same key produce one attempt and one charge |
| `payment_idempotency::retry_returns_the_same_response_without_calling_the_psp_again` | Replays return an identical body, and the PSP request count stays at 1 |
| `payment_psp_failures::psp_timeout_returns_pending_quickly_...` | `tok_timeout` returns `202` fast, the invoice stays `open` and blocks new payments and `void`, and the reconciler then marks it `paid` with one charge |
| `payment_psp_failures::psp_outage_never_leaves_the_invoice_stuck` | `tok_network_error` gives up after the re-check budget, and the invoice is payable again |
| `payment_psp_failures::crash_between_charge_and_settle_...` | The PSP charged but we never recorded it; recovery happens without a second charge |

There are also unit tests for the state machine (every state × action pair), the money arithmetic, and webhook signing (against the Standard Webhooks test vector).

To run everything without a local Rust toolchain:

```bash
docker compose --profile test run --rm tests
```

Or, with Rust installed and the compose database running (`docker compose up -d db`):

```bash
DATABASE_URL=postgres://invoices:invoices@localhost:5433/invoices cargo test
```

Each test gets its own throwaway database via `#[sqlx::test]`.

## Layout

```
src/
  invoices/        state machine, pricing, handlers
  payments/        pay flow, PSP client, reconciler
  webhooks/        outbox, dispatcher, signing, handlers
  api_keys.rs      key issuing, hashing, auth extractor
  ...
migrations/        SQL migrations (embedded, run on startup)
tests/             integration tests
mock-psp/          the mock payment processor (lib + bin), a workspace member
webhook-sink/      demo webhook receiver, a workspace member
```

## Configuration

| Variable | Default | |
|---|---|---|
| `DATABASE_URL` | (required) | |
| `LISTEN_ADDR` | `0.0.0.0:8080` | |
| `PSP_BASE_URL` | `http://localhost:9090` | |
| `PSP_TIMEOUT_MS` | `5000` | Longest we wait for the PSP before answering `202` |
| `WEBHOOK_TIMEOUT_MS` | `10000` | Per-delivery request timeout |
| `WORKER_POLL_INTERVAL_MS` | `1000` | How often idle workers look for due work |
| `RUST_LOG` | `info,sqlx=warn` | |
