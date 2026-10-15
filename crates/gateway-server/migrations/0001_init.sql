-- Every verified webhook body, stored verbatim BEFORE any processing.
-- The webhook handler only does this insert, then returns 200.
CREATE TABLE raw_events (
    id           BIGSERIAL PRIMARY KEY,
    received_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    body         TEXT        NOT NULL,
    processed_at TIMESTAMPTZ,
    attempts     INT         NOT NULL DEFAULT 0,
    last_error   TEXT
);
CREATE INDEX raw_events_pending ON raw_events (id) WHERE processed_at IS NULL;

-- Tenant routing: which workspace owns which WhatsApp business number.
CREATE TABLE workspaces (
    phone_number_id TEXT PRIMARY KEY,
    workspace_id    TEXT NOT NULL,
    display_name    TEXT
);

-- Normalized, de-duplicated events. UNIQUE(dedupe_key) is the idempotency
-- guarantee: Meta delivers at-least-once.
CREATE TABLE events (
    id               BIGSERIAL PRIMARY KEY,
    dedupe_key       TEXT        NOT NULL UNIQUE,
    raw_event_id     BIGINT      REFERENCES raw_events (id) ON DELETE SET NULL,
    phone_number_id  TEXT        NOT NULL,
    workspace_id     TEXT,
    kind             TEXT        NOT NULL,
    payload          JSONB       NOT NULL,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- delivery to the sink (echo bot / main app)
    forwarded_at     TIMESTAMPTZ,
    forward_attempts INT         NOT NULL DEFAULT 0,
    forward_error    TEXT,
    next_attempt_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX events_undelivered ON events (id) WHERE forwarded_at IS NULL;
CREATE INDEX events_raw ON events (raw_event_id);

-- 24h customer service window tracker.
CREATE TABLE conversations (
    phone_number_id TEXT   NOT NULL,
    customer        TEXT   NOT NULL,
    last_inbound_at BIGINT NOT NULL,
    PRIMARY KEY (phone_number_id, customer)
);

-- Outbound messages and their delivery status. Rows may be created by the
-- status webhook before the send path records them (race), so most columns
-- are nullable.
CREATE TABLE outbound_messages (
    wamid            TEXT PRIMARY KEY,
    phone_number_id  TEXT        NOT NULL,
    recipient        TEXT,
    kind             TEXT,
    request          JSONB,
    status           TEXT,
    status_at        BIGINT,
    billable         BOOLEAN,
    pricing_category TEXT,
    error_code       BIGINT,
    error_title      TEXT,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX outbound_recent ON outbound_messages (created_at DESC);

-- Downloaded inbound media (receipts, voice notes). Meta media URLs expire
-- in minutes, so we fetch immediately.
CREATE TABLE media (
    media_id        TEXT PRIMARY KEY,
    phone_number_id TEXT        NOT NULL,
    mime_type       TEXT,
    size            BIGINT      NOT NULL,
    sha256          TEXT        NOT NULL,
    data            BYTEA       NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
