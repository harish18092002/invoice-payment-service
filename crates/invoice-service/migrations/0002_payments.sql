CREATE TABLE payment_attempts (
    id            uuid PRIMARY KEY,
    invoice_id    uuid NOT NULL REFERENCES invoices (id),
    business_id   uuid NOT NULL REFERENCES businesses (id),
    amount_cents  bigint NOT NULL
                  CHECK (amount_cents > 0),
    card_token    text NOT NULL,
    status        text NOT NULL
                  CHECK (status IN ('pending', 'succeeded', 'failed')),
    psp_ref       text,
    failure_code  text,
    created_at    timestamptz NOT NULL DEFAULT now(),
    completed_at  timestamptz
);

CREATE UNIQUE INDEX attempts_one_pending ON payment_attempts (invoice_id) WHERE status = 'pending';
CREATE UNIQUE INDEX attempts_one_success ON payment_attempts (invoice_id) WHERE status = 'succeeded';
CREATE INDEX attempts_stale ON payment_attempts (created_at) WHERE status = 'pending';

CREATE TABLE idempotency_keys (
    business_id      uuid NOT NULL REFERENCES businesses (id),
    key              text NOT NULL,
    request_hash     bytea NOT NULL,
    attempt_id       uuid REFERENCES payment_attempts (id),
    response_status  int,
    response_body    jsonb,
    created_at       timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (business_id, key)
);
