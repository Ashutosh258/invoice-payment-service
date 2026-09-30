# AI Usage

## Tools

- **Claude Code** (Anthropic's coding agent, Claude Sonnet 5.5), used through the Claude desktop app.

## How AI Was Used

AI was used primarily as a development and review assistant throughout the project.

Specifically, I used AI for:

- **Bug finding and debugging:** Reviewing the implementation to identify bugs, edge cases, incorrect behavior, and failure scenarios.
- **Syntax and compilation corrections:** Fixing Rust syntax issues, compiler errors, type mismatches, and implementation mistakes.
- **Code review:** Reviewing existing implementations and suggesting improvements to error handling, concurrency handling, tests, and code structure.
- **README improvements:** Correcting and improving the README, including setup instructions, examples, formatting, and inconsistencies.
- **Documentation corrections:** Reviewing `DESIGN.md` and `docs/API.md` for incorrect or unclear details and fixing documentation that no longer matched the implementation.
- **Test debugging:** Investigating failing tests and helping identify their underlying causes.
- **Code cleanup:** Suggesting small refactors and improvements to existing code.
- **Validation:** Helping identify additional scenarios to test and verifying behavior through the test suite and Docker environment.

AI suggestions were reviewed before being incorporated. Changes affecting application behavior were verified through tests or by running the application.

## Issues Found and Corrected

### 1. Test pool configuration

The initial test setup attempted to use a 30-connection pool with `#[sqlx::test]`. This conflicted with SQLx's shared test pool limit and caused integration tests to fail.

The test setup was changed to use a separate pool against the per-test database.

### 2. `unreachable!()` in a request path

An `unreachable!()` was found in the invoice transition helper even though the associated path could be reached during normal request handling.

The helper was refactored so callers explicitly provide the relevant event type.

### 3. Documentation length

The initial `DESIGN.md` was longer than the requested 800–1,500 word range.

It was reviewed and shortened while keeping the required design decisions and failure-mode explanations.

### 4. Webhook log readability

The webhook sink logged JSONB payloads directly. PostgreSQL JSONB does not preserve key ordering, which made important fields such as the event `type` difficult to find in the logs.

The logging was adjusted so that the webhook event type is logged as a separate field.

## Correctness Checks

### Concurrency

The row lock and pending-attempt check were temporarily removed and the concurrency test was run again. The partial unique index still prevented duplicate charges.

The original protections were then restored.

### Test stability

The payment test suites were run five consecutive times without failures.

### Webhook signing

Webhook signing was checked against the published Standard Webhooks test vector. The Python verification example documented in `docs/API.md` was also checked against the same vector.

### PSP timeout

The `tok_timeout` flow was tested against the Docker Compose stack.

The request returned `202` after approximately five seconds. The first re-check returned `409` while the payment was still in flight, and the following re-check recorded the successful result. The invoice ultimately reached `paid` with one charge.

## Three Decisions I Made Myself

### 1. A PSP timeout means "we don't know", not "it failed"

**What I chose:** Every PSP response is classified into one of three outcomes:

- **Succeeded**
- **Definitely failed:** a decline, a 4xx, or connection refused, which I treat as an outcome where the charge did not reach the PSP.
- **Unknown:** a timeout, a 5xx, a 409, or a dropped connection.

An unknown outcome leaves the payment attempt as `pending` and returns `202`. A background reconciler then resends the same request with the same PSP idempotency key, about six times over approximately 19 minutes, until the PSP gives a definite answer.

If it never does, the attempt is closed as `failed/psp_unavailable` and the invoice becomes payable again.

**What I rejected:**

- **Treating a timeout as a failure:** This is simpler, but the original request may have reached the PSP and charged the customer. A retry could therefore create a double charge.
- **Keeping the attempt pending forever:** This is safer from a duplicate-charge perspective, but a prolonged PSP outage would permanently block the invoice.

**Why:** A PSP that successfully processed the charge should return the same result when the same idempotency key is replayed. After several replays with no successful result, treating the attempt as unavailable and allowing the invoice to become payable again is a practical boundary.

The remaining risk belongs to settlement reconciliation, which I documented as a production gap.

### 2. The idempotency key lives on the payment attempt, not in its own table

**What I chose:** I did not create a separate `idempotency_keys` table.

`payment_attempts` has:

`UNIQUE (business_id, idempotency_key)`

This means an idempotency key maps to exactly one payment attempt.

A retry returns that attempt's current state:

- `202` while pending
- `200` once successful
- `402` once the payment has failed

Requests rejected before an attempt exists, such as a nonexistent invoice, a non-payable invoice, or an already-running payment, are not recorded because they did not create an attempt or change payment state.

**What I rejected:** I initially considered a Stripe-style separate table that caches response bodies and expires idempotency keys after 24 hours.

I dropped that approach because its separate state could become another source of truth and the claim-then-fill-in flow introduced unnecessary complexity around the final response state.

**Trade-offs:** Idempotency keys never expire, and this idempotency guarantee applies to the `/pay` operation only.

### 3. Concurrency: lock the invoice row, back it with unique indexes, and never hold a lock during the PSP call

**What I chose:**

- `SELECT ... FOR UPDATE` is used on the invoice while starting and settling a payment.
- Two partial unique indexes provide a database-level backstop:
  - one pending attempt per invoice
  - one successful attempt per invoice
- While the PSP call is in progress, the durable `pending` payment attempt prevents another payment from starting instead of holding a database lock.
- There is no separate `processing` invoice state.

**What I rejected:**

- **Advisory locks:** They are separate from the data they protect.
- **Serializable isolation:** It would introduce retry loops across transactions without providing enough benefit for this case.
- **Optimistic version numbers:** The payment check spans the invoice and payment-attempt data.
- **Holding the lock during the PSP call:** A 30-second PSP timeout would hold a database connection and block other operations on that invoice.
- **A `processing` invoice state:** Returning from it would require the state machine to remember whether the invoice was previously `open` or `uncollectible`.

**Evidence:** I temporarily removed the invoice row lock and the pending-attempt check and re-ran the 20-client concurrency test. The test still passed because the unique index alone prevented duplicate successful/pending attempts.

The original locking and application-level checks were then restored to keep both the transactional behavior and the database-level invariant.

## One Thing AI Got Wrong

One concrete issue I identified while reviewing the AI-assisted implementation was the use of an `unreachable!()` branch in the invoice transition helper.

Although the branch was initially treated as unreachable, the corresponding request path could occur during normal application behavior. I changed the helper so that callers explicitly provide the event type instead of relying on the unreachable branch.

I verified the correction by rebuilding the service and running the relevant test suite.