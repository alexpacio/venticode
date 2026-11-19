-- Additional indexes for cascades, privacy export/erase, and managed computer reconciliation.
CREATE INDEX IF NOT EXISTS mentions_conversation ON mentions(conversation_id)
    WHERE conversation_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS mentions_owner ON mentions(owner_id);
CREATE INDEX IF NOT EXISTS link_challenges_connection ON link_challenges(connection_id);
CREATE INDEX IF NOT EXISTS external_links_user ON external_links(user_id);
CREATE INDEX IF NOT EXISTS conversations_bot_connection ON conversations(bot_connection)
    WHERE bot_connection IS NOT NULL;
CREATE INDEX IF NOT EXISTS standalone_sessions_bot_grant ON standalone_sessions(bot_grant)
    WHERE bot_grant IS NOT NULL;
CREATE INDEX IF NOT EXISTS projects_harness ON projects(harness_id);

-- Bound link_notices retention so it can be swept periodically.
ALTER TABLE link_notices ADD COLUMN IF NOT EXISTS created_at TIMESTAMPTZ NOT NULL DEFAULT now();
CREATE INDEX IF NOT EXISTS link_notices_created ON link_notices(created_at);

-- Persist mention retry counts in Postgres instead of a volatile in-memory map.
ALTER TABLE mentions ADD COLUMN IF NOT EXISTS retry_count INTEGER NOT NULL DEFAULT 0;

-- Enforce an absolute maximum lifetime on standalone sessions even when refreshed.
ALTER TABLE standalone_sessions ADD COLUMN IF NOT EXISTS absolute_expires_at TIMESTAMPTZ NOT NULL DEFAULT now() + interval '7 days';
