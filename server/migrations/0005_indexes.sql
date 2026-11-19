CREATE INDEX IF NOT EXISTS mentions_worker_poll ON mentions(state, lease_until, created_at)
    WHERE state IN ('pending', 'answering', 'sending');
CREATE INDEX IF NOT EXISTS mentions_connection_created ON mentions(connection_id, created_at DESC);
CREATE INDEX IF NOT EXISTS object_cleanup_due ON object_cleanup(due_at);
CREATE INDEX IF NOT EXISTS conversations_active_lease ON conversations(lease_until)
    WHERE active_run IS NOT NULL;
CREATE INDEX IF NOT EXISTS approvals_run ON approvals(run_id);
CREATE INDEX IF NOT EXISTS approvals_conversation ON approvals(conversation_id);
CREATE INDEX IF NOT EXISTS interactive_calls_run ON interactive_calls(run_id);
CREATE INDEX IF NOT EXISTS interactive_calls_conversation ON interactive_calls(conversation_id);
CREATE INDEX IF NOT EXISTS link_challenges_expires ON link_challenges(expires_at);
CREATE INDEX IF NOT EXISTS connections_workspace ON connections(workspace);
CREATE INDEX IF NOT EXISTS connections_project ON connections(project_id)
    WHERE project_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS messages_conversation ON messages(conversation_id);
CREATE INDEX IF NOT EXISTS managed_computers_touched ON managed_computers(touched_at);
CREATE INDEX IF NOT EXISTS computer_model_calls_run ON computer_model_calls(run_id);
