CREATE TABLE webhook_endpoints (
    id           uuid PRIMARY KEY,
    business_id  uuid NOT NULL REFERENCES businesses (id),
    url          text NOT NULL,
    secret       text NOT NULL,
    enabled      boolean NOT NULL DEFAULT true,
    created_at   timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE events (
    id           uuid PRIMARY KEY,
    business_id  uuid NOT NULL REFERENCES businesses (id),
    type         text NOT NULL,
    payload      jsonb NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX events_list ON events (business_id, id);

CREATE TABLE webhook_deliveries (
    id                uuid PRIMARY KEY,
    event_id          uuid NOT NULL REFERENCES events (id),
    endpoint_id       uuid NOT NULL REFERENCES webhook_endpoints (id),
    status            text NOT NULL DEFAULT 'pending'
                      CHECK (status IN ('pending', 'delivered', 'dead')),
    attempt_count     int NOT NULL DEFAULT 0,
    next_attempt_at   timestamptz NOT NULL DEFAULT now(),
    last_status_code  int,
    last_error        text,
    delivered_at      timestamptz,
    UNIQUE (event_id, endpoint_id)
);

CREATE INDEX deliveries_due ON webhook_deliveries (next_attempt_at) WHERE status = 'pending';
