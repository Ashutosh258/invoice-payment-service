CREATE TABLE payment_attempts (
    id               UUID        PRIMARY KEY,
    business_id      UUID        NOT NULL REFERENCES businesses (id),
    invoice_id       UUID        NOT NULL REFERENCES invoices (id),
    status           TEXT        NOT NULL CHECK (status IN ('pending', 'succeeded', 'failed')),
    amount_cents     BIGINT      NOT NULL CHECK (amount_cents > 0),
    currency         TEXT        NOT NULL DEFAULT 'usd' CHECK (currency = 'usd'),
    card_token       TEXT        NOT NULL,
    idempotency_key  TEXT        NOT NULL,
    psp_ref          TEXT,
    failure_code     TEXT,
    failure_message  TEXT,
    recheck_count    INTEGER     NOT NULL DEFAULT 0,
    next_recheck_at  TIMESTAMPTZ,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at     TIMESTAMPTZ,

    CONSTRAINT payment_attempts_idempotency_key UNIQUE (business_id, idempotency_key),

    CHECK (status <> 'succeeded' OR psp_ref IS NOT NULL),
    CHECK ((status = 'failed') = (failure_code IS NOT NULL)),
    CHECK ((status = 'pending') = (next_recheck_at IS NOT NULL)),
    CHECK ((status = 'pending') = (completed_at IS NULL))
);

CREATE UNIQUE INDEX payment_attempts_one_pending_per_invoice
    ON payment_attempts (invoice_id) WHERE status = 'pending';
CREATE UNIQUE INDEX payment_attempts_one_success_per_invoice
    ON payment_attempts (invoice_id) WHERE status = 'succeeded';

CREATE INDEX payment_attempts_invoice_id_idx ON payment_attempts (invoice_id, id);

CREATE INDEX payment_attempts_due_recheck_idx
    ON payment_attempts (next_recheck_at) WHERE status = 'pending';
