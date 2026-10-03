CREATE TABLE businesses (
    id          uuid PRIMARY KEY,
    name        text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE api_keys (
    id           uuid PRIMARY KEY,
    business_id  uuid NOT NULL REFERENCES businesses (id),
    prefix       text NOT NULL UNIQUE,
    key_hash     bytea NOT NULL,
    label        text,
    created_at   timestamptz NOT NULL DEFAULT now(),
    last_used_at timestamptz,
    revoked_at   timestamptz
);

CREATE TABLE customers (
    id           uuid PRIMARY KEY,
    business_id  uuid NOT NULL REFERENCES businesses (id),
    name         text NOT NULL,
    email        text NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    UNIQUE (id, business_id)
);

CREATE INDEX customers_list ON customers (business_id, created_at DESC, id DESC);

CREATE TABLE invoices (
    id           uuid PRIMARY KEY,
    business_id  uuid NOT NULL REFERENCES businesses (id),
    customer_id  uuid NOT NULL,
    state        text NOT NULL DEFAULT 'draft'
                 CHECK (state IN ('draft', 'open', 'paid', 'void', 'uncollectible')),
    currency     char(3) NOT NULL DEFAULT 'USD'
                 CHECK (currency = 'USD'),
    total_cents  bigint NOT NULL
                 CHECK (total_cents > 0),
    due_date     date NOT NULL,
    paid_at      timestamptz,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    FOREIGN KEY (customer_id, business_id) REFERENCES customers (id, business_id)
);

CREATE INDEX invoices_list ON invoices (business_id, state, created_at DESC, id DESC);
CREATE INDEX invoices_customer ON invoices (customer_id);

CREATE TABLE invoice_line_items (
    id                 uuid PRIMARY KEY,
    invoice_id         uuid NOT NULL REFERENCES invoices (id),
    position           int NOT NULL,
    description        text NOT NULL,
    quantity           int NOT NULL
                       CHECK (quantity > 0),
    unit_amount_cents  bigint NOT NULL
                       CHECK (unit_amount_cents >= 0),
    UNIQUE (invoice_id, position)
);
