CREATE TABLE managed_computers (
    workspace uuid NOT NULL,
    owner_id uuid NOT NULL,
    id uuid NOT NULL UNIQUE,
    touched_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace, owner_id)
);
CREATE TRIGGER protect_computer BEFORE INSERT ON managed_computers
    FOR EACH ROW EXECUTE FUNCTION protect_subject();

ALTER TABLE connections ADD COLUMN runtime text NOT NULL DEFAULT 'project'
    CHECK (runtime IN ('project', 'kitesurf', 'computer'));

ALTER TABLE conversations ALTER COLUMN project_id DROP NOT NULL;
ALTER TABLE conversations ADD COLUMN runtime text NOT NULL DEFAULT 'project',
    ADD COLUMN browser_workspace uuid, ADD COLUMN browser_owner uuid,
    ADD CONSTRAINT conversation_runtime CHECK (
        (runtime = 'kitesurf' AND from_bot AND project_id IS NULL
            AND browser_workspace IS NOT NULL AND browser_owner IS NOT NULL)
        OR (runtime IN ('project', 'computer') AND project_id IS NOT NULL
            AND browser_workspace IS NULL AND browser_owner IS NULL));

DROP VIEW conversation_json;
CREATE VIEW conversation_json AS
    SELECT c.*, coalesce(p.workspace, c.browser_workspace) AS workspace,
        coalesce(p.owner_id, c.browser_owner) AS owner_id, p.harness_id, p.root, p.path,
        (to_jsonb(c) - 'active_run' - 'lease_until' - 'bot_connection' - 'from_bot'
            - 'browser_workspace' - 'browser_owner') || jsonb_build_object('harness_id',p.harness_id) AS document
    FROM conversations c LEFT JOIN projects p ON p.id = c.project_id;

CREATE INDEX browser_conversation_owner ON conversations(browser_workspace, browser_owner)
    WHERE runtime = 'kitesurf';

CREATE TABLE computer_model_calls (
    run_id uuid NOT NULL REFERENCES messages(run_id) ON DELETE CASCADE,
    id uuid PRIMARY KEY,
    finished boolean NOT NULL DEFAULT false
);
