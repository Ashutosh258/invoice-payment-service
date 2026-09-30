CREATE TABLE customers (
    id           UUID        PRIMARY KEY,
    business_id  UUID        NOT NULL REFERENCES businesses (id),
    name         TEXT        NOT NULL CHECK (length(name) BETWEEN 1 AND 200),
    email        TEXT        NOT NULL CHECK (length(email) BETWEEN 3 AND 254),
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT customers_business_id_id_key UNIQUE (business_id, id)
);

CREATE TABLE invoices (
    id                       UUID        PRIMARY KEY,
    business_id              UUID        NOT NULL REFERENCES businesses (id),
    customer_id              UUID        NOT NULL,
    status                   TEXT        NOT NULL
                                         CHECK (status IN ('draft', 'open', 'paid', 'void', 'uncollectible')),
    currency                 TEXT        NOT NULL DEFAULT 'usd' CHECK (currency = 'usd'),
    total_cents              BIGINT      NOT NULL CHECK (total_cents > 0),
    due_date                 DATE        NOT NULL,
    created_at               TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at               TIMESTAMPTZ NOT NULL DEFAULT now(),
    finalized_at             TIMESTAMPTZ,
    paid_at                  TIMESTAMPTZ,
    voided_at                TIMESTAMPTZ,
    marked_uncollectible_at  TIMESTAMPTZ,

    FOREIGN KEY (business_id, customer_id) REFERENCES customers (business_id, id),

    CHECK ((status = 'paid') = (paid_at IS NOT NULL)),
    CHECK ((status = 'void') = (voided_at IS NOT NULL))
);

CREATE INDEX invoices_business_id_idx        ON invoices (business_id, id);
CREATE INDEX invoices_business_id_status_idx ON invoices (business_id, status, id);
CREATE INDEX invoices_customer_id_idx        ON invoices (customer_id);

CREATE TABLE invoice_line_items (
    id                 UUID    PRIMARY KEY,
    invoice_id         UUID    NOT NULL REFERENCES invoices (id),
    position           INTEGER NOT NULL CHECK (position >= 0),
    description        TEXT    NOT NULL CHECK (length(description) BETWEEN 1 AND 500),
    quantity           BIGINT  NOT NULL CHECK (quantity > 0),
    unit_amount_cents  BIGINT  NOT NULL CHECK (unit_amount_cents >= 0),
    amount_cents       BIGINT  NOT NULL,

    CHECK (amount_cents = quantity * unit_amount_cents),
    UNIQUE (invoice_id, position)
);
