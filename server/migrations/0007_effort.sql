ALTER TABLE conversations ADD COLUMN effort text NOT NULL DEFAULT 'medium'
    CHECK (effort IN ('none', 'low', 'medium', 'high'));

-- Recreate the view so it picks up the new column.
DROP VIEW IF EXISTS conversation_json;
CREATE VIEW conversation_json AS
    SELECT c.*, coalesce(p.workspace, c.browser_workspace) AS workspace,
        coalesce(p.owner_id, c.browser_owner) AS owner_id, p.harness_id, p.root, p.path,
        (to_jsonb(c) - 'active_run' - 'lease_until' - 'bot_connection' - 'from_bot'
            - 'browser_workspace' - 'browser_owner') || jsonb_build_object('harness_id',p.harness_id) AS document
    FROM conversations c LEFT JOIN projects p ON p.id = c.project_id;