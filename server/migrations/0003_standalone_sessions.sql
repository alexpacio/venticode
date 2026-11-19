ALTER TABLE connections ADD COLUMN standalone_credentials bytea;

CREATE TABLE standalone_bot_grants (
    id uuid PRIMARY KEY,
    connection_id uuid NOT NULL REFERENCES connections ON DELETE CASCADE,
    owner_id uuid NOT NULL,
    UNIQUE (connection_id, owner_id)
);

CREATE TABLE standalone_sessions (
    token_hash text PRIMARY KEY,
    workspace uuid NOT NULL,
    owner_id uuid NOT NULL,
    expires_at timestamptz NOT NULL,
    bot_grant uuid REFERENCES standalone_bot_grants ON DELETE CASCADE
);

CREATE INDEX standalone_sessions_expiry ON standalone_sessions(expires_at);

CREATE TABLE standalone_runner_inventory (
    workspace uuid NOT NULL,
    runner_id uuid NOT NULL,
    roots jsonb NOT NULL,
    host_info jsonb NOT NULL,
    PRIMARY KEY (workspace, runner_id)
);
