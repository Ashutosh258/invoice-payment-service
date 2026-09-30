CREATE TABLE events (
    id           UUID        PRIMARY KEY,
    business_id  UUID        NOT NULL REFERENCES businesses (id),
    event_type   TEXT        NOT NULL,
    payload      JSONB       NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX events_business_id_idx ON events (business_id, id);

CREATE TABLE webhook_endpoints (
    id           UUID        PRIMARY KEY,
    business_id  UUID        NOT NULL REFERENCES businesses (id),
    url          TEXT        NOT NULL,
    secret       TEXT        NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    disabled_at  TIMESTAMPTZ
);

CREATE INDEX webhook_endpoints_business_id_idx
    ON webhook_endpoints (business_id) WHERE disabled_at IS NULL;

CREATE TABLE webhook_deliveries (
    id                    UUID        PRIMARY KEY,
    event_id              UUID        NOT NULL REFERENCES events (id),
    endpoint_id           UUID        NOT NULL REFERENCES webhook_endpoints (id),
    status                TEXT        NOT NULL DEFAULT 'pending'
                                      CHECK (status IN ('pending', 'succeeded', 'failed', 'canceled')),
    attempt_count         INTEGER     NOT NULL DEFAULT 0,
    next_attempt_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_response_status  INTEGER,
    last_error            TEXT,
    delivered_at          TIMESTAMPTZ,
    created_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT now(),

    UNIQUE (event_id, endpoint_id)
);

CREATE INDEX webhook_deliveries_due_idx
    ON webhook_deliveries (next_attempt_at) WHERE status = 'pending';
CREATE INDEX webhook_deliveries_endpoint_id_idx
    ON webhook_deliveries (endpoint_id, id);
