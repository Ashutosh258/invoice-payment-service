CREATE TABLE businesses (
    id          UUID        PRIMARY KEY,
    name        TEXT        NOT NULL CHECK (length(name) BETWEEN 1 AND 200),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE api_keys (
    id           UUID        PRIMARY KEY,
    business_id  UUID        NOT NULL REFERENCES businesses (id),
    key_prefix   TEXT        NOT NULL,
    key_hash     BYTEA       NOT NULL UNIQUE,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at   TIMESTAMPTZ
);

CREATE INDEX api_keys_business_id_idx ON api_keys (business_id);
