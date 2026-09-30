# API reference

Base URL: `http://localhost:8080`. Every endpoint except `/health` is under `/v1` and needs an API key.

## Conventions

**Authentication:** `Authorization: Bearer sk_...`. A missing, unknown, or revoked key gets `401`.

**Money:** always integer cents (`*_cents`, JSON integers). Currency is always `usd`. A fractional value is rejected with `422`.

**Ids:** UUIDs (v7, so they sort by creation time).

**Timestamps:** RFC 3339, UTC.

**Unknown fields** in request bodies are rejected with `422`, so a stray `total_cents` fails loudly instead of being silently ignored.

**Pagination:** list endpoints return newest first and take `limit` (1–100, default 20) and `starting_after=<id>`. The response looks like this:

```json
{ "data": [ ... ], "has_more": true }
```

To get the next page, pass the last item's `id` as `starting_after`.

**Request ids:** every response carries `x-request-id`. Send your own to have it propagated into our logs.

### Errors

Every error, from validation to a 500, has the same shape:

```json
{
  "error": {
    "code": "invoice_already_paid",
    "message": "this invoice has already been paid",
    "details": { }
  }
}
```

`code` is stable, so switch on it. `message` is for humans. `details` is only present when it adds something.

| HTTP | code | When |
|---|---|---|
| 400 | `invalid_request` | Malformed JSON, bad path or query parameter, missing Idempotency-Key |
| 401 | `unauthorized` | No API key, or an invalid or revoked one |
| 402 | `payment_failed` | The PSP declined the payment. `details.payment_attempt` has the attempt |
| 404 | `not_found` | Doesn't exist, or belongs to another business (we don't distinguish) |
| 409 | `invalid_state_transition` | The transition isn't allowed from the invoice's current status |
| 409 | `invoice_already_paid` | `POST /pay` on a paid invoice |
| 409 | `invoice_not_payable` | `POST /pay` on a draft or void invoice |
| 409 | `payment_in_progress` | Another payment for this invoice is pending. `details.payment_attempt_id` |
| 415 | `invalid_request` | Missing `Content-Type: application/json` |
| 422 | `validation_failed` | The body parsed but its values are invalid |
| 422 | `idempotency_key_reused` | Idempotency-Key was already used for a different request |
| 500 | `internal_error` | Our fault. Details are in our logs, keyed by `x-request-id` |

## Customers

### `POST /v1/customers`

```json
{ "name": "Ada Lovelace", "email": "ada@example.com" }
```

Returns `201`:

```json
{ "id": "01a0...", "name": "Ada Lovelace", "email": "ada@example.com", "created_at": "2026-09-30T17:40:58Z" }
```

### `GET /v1/customers/{id}`

Returns the customer, or `404`.

### `GET /v1/customers?limit=&starting_after=`

Returns a page of customers.

## Invoices

Invoice object:

```json
{
  "id": "01a0...",
  "customer_id": "01a0...",
  "status": "open",
  "currency": "usd",
  "total_cents": 54400,
  "due_date": "2026-12-31",
  "line_items": [
    { "id": "01a0...", "description": "Pro plan (annual)", "quantity": 1, "unit_amount_cents": 49900, "amount_cents": 49900 },
    { "id": "01a0...", "description": "Extra seats", "quantity": 3, "unit_amount_cents": 1500, "amount_cents": 4500 }
  ],
  "created_at": "...",
  "updated_at": "...",
  "finalized_at": "...",
  "paid_at": null,
  "voided_at": null,
  "marked_uncollectible_at": null
}
```

`status` is one of `draft`, `open`, `paid`, `void`, or `uncollectible`. See DESIGN.md §2 for the state machine.

### `POST /v1/invoices`

```json
{
  "customer_id": "01a0...",
  "due_date": "2026-12-31",
  "line_items": [ { "description": "Pro plan", "quantity": 1, "unit_amount_cents": 49900 } ]
}
```

Returns `201` with the invoice in `draft`. The server computes `amount_cents` and `total_cents`. The limits are:

- 1 to 100 line items
- `quantity` from 1 to 1,000,000
- `unit_amount_cents` from 0 to 100,000,000
- a total greater than 0 and no more than 1,000,000,000
- `due_date` not in the past

It emits `invoice.created`.

### `GET /v1/invoices/{id}`

Returns the invoice.

### `GET /v1/invoices?status=open&customer_id=&limit=&starting_after=`

Returns a page of invoices. Both filters are optional.

### Transitions

Each of these returns `200` with the updated invoice, or `409`:

| Endpoint | From → to | Event |
|---|---|---|
| `POST /v1/invoices/{id}/finalize` | draft → open | `invoice.finalized` |
| `POST /v1/invoices/{id}/void` | draft, open, uncollectible → void | `invoice.voided` |
| `POST /v1/invoices/{id}/mark_uncollectible` | open → uncollectible | `invoice.marked_uncollectible` |

`void` and `mark_uncollectible` return `409 payment_in_progress` while a payment attempt is pending.

## Payments

### `POST /v1/invoices/{id}/pay`

Headers: `Idempotency-Key: <1–255 chars>` (**required**).

```json
{ "card_token": "tok_success" }
```

There is no amount field. A payment is always for the invoice total. The invoice must be `open` or `uncollectible`.

| Status | Meaning | Body |
|---|---|---|
| `200` | Payment succeeded; the invoice is now `paid` | payment attempt |
| `202` | The PSP didn't answer definitively in time. The attempt is `pending` and resolves in the background | payment attempt |
| `402` | Declined. The invoice stays payable, so retry with a **new** key | error with `details.payment_attempt` |
| `409` | `invoice_already_paid`, `invoice_not_payable`, or `payment_in_progress` | error |
| `422` | `idempotency_key_reused` | error |

Payment attempt object:

```json
{
  "id": "01a0...",
  "invoice_id": "01a0...",
  "status": "succeeded",
  "amount_cents": 54400,
  "currency": "usd",
  "idempotency_key": "order-1234",
  "psp_ref": "79f8c947-...",
  "failure_code": null,
  "failure_message": null,
  "created_at": "...",
  "updated_at": "...",
  "completed_at": "..."
}
```

`status` is `pending`, `succeeded`, or `failed`. `failure_code` is one of `card_declined`, `insufficient_funds`, `invalid_card_token`, `processing_error`, or `psp_unavailable`.

**Idempotency.** Retrying with the same key never charges again. It returns the attempt that key created, in its current state, with the header `Idempotent-Replayed: true`. So a `202` retried later becomes a `200` or `402` once the attempt settles. Keys are scoped to your business and never expire.

### `GET /v1/invoices/{id}/payment_attempts`

Returns all attempts for the invoice, oldest first.

## Webhooks

### `POST /v1/webhook_endpoints`

```json
{ "url": "https://example.com/hooks" }
```

Returns `201` with `{ id, url, created_at, disabled_at, secret }`. **`secret` is only returned here.**

### Managing endpoints

- `GET /v1/webhook_endpoints` lists your endpoints (without secrets).
- `DELETE /v1/webhook_endpoints/{id}` disables an endpoint and cancels its queued deliveries.
- `GET /v1/webhook_endpoints/{id}/deliveries?type=&limit=&starting_after=` is the delivery log: `status` (`pending`, `succeeded`, `failed`, or `canceled`), `attempt_count`, `last_response_status`, `last_error`, and `next_attempt_at`.

### Events

`GET /v1/events?type=invoice.paid&limit=&starting_after=` returns every event, in exactly the envelope that was delivered. Use it to catch up after downtime.

Event types:

- `invoice.created`
- `invoice.finalized`
- `invoice.paid`
- `invoice.payment_failed`
- `invoice.voided`
- `invoice.marked_uncollectible`

Envelope:

```json
{
  "id": "01a0...",
  "type": "invoice.paid",
  "created_at": "...",
  "data": { "invoice": { ... }, "payment_attempt": { ... } }
}
```

`payment_attempt` is only present on `invoice.paid` and `invoice.payment_failed`.

### Verifying a delivery

We follow [Standard Webhooks](https://www.standardwebhooks.com). Each delivery has these headers:

```
webhook-id:        <event id>            stable across retries: dedupe on it
webhook-timestamp: <unix seconds>        reject if more than 5 minutes from now
webhook-signature: v1,<base64 signature>
```

The signature is `base64(HMAC-SHA256(key, "{webhook-id}.{webhook-timestamp}.{raw body}"))`, where `key` is the base64-decoded part of your secret after `whsec_`. In Python:

```python
import base64, hashlib, hmac, time

def verify(secret: str, headers: dict, body: bytes) -> bool:
    msg_id, ts = headers["webhook-id"], headers["webhook-timestamp"]
    if abs(time.time() - int(ts)) > 300:
        return False
    key = base64.b64decode(secret.removeprefix("whsec_"))
    expected = base64.b64encode(hmac.new(key, f"{msg_id}.{ts}.".encode() + body, hashlib.sha256).digest()).decode()
    return any(hmac.compare_digest(expected, sig.removeprefix("v1,"))
               for sig in headers["webhook-signature"].split())
```

Any Standard Webhooks library (such as `standardwebhooks` or `svix`) verifies these deliveries as they are.

## API keys

- `POST /v1/api_keys` returns `201` with `{ id, prefix, created_at, revoked_at, secret }`. The secret is shown once.
- `GET /v1/api_keys` lists keys by prefix only.
- `DELETE /v1/api_keys/{id}` revokes a key immediately.

The first key for a business comes from the CLI: `invoice-service create-business "<name>"`.

## Health

`GET /health` returns `200` when the database is reachable and `503` otherwise.
