CREATE TABLE projects (
    id uuid PRIMARY KEY, workspace uuid NOT NULL, owner_id uuid NOT NULL,
    harness_id uuid NOT NULL, root text NOT NULL, path text NOT NULL, name text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(), UNIQUE(workspace,owner_id,harness_id,root,path)
);
CREATE TABLE conversations (
    id uuid PRIMARY KEY, project_id uuid NOT NULL REFERENCES projects ON DELETE CASCADE,
    title text NOT NULL, model text NOT NULL, permission_mode text NOT NULL CHECK(permission_mode IN ('ask','auto_edit','plan')),
    state text NOT NULL DEFAULT 'idle' CHECK(state IN ('idle','running','waiting','failed')),
    revision bigint NOT NULL DEFAULT 1, created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(), active_run uuid, lease_until timestamptz
);
CREATE INDEX conversations_project ON conversations(project_id, updated_at DESC);
CREATE TABLE messages (
    workspace uuid NOT NULL, owner_id uuid NOT NULL, id uuid NOT NULL, conversation_id uuid NOT NULL REFERENCES conversations ON DELETE CASCADE,
    fingerprint text NOT NULL, run_id uuid NOT NULL UNIQUE, outcome text NOT NULL DEFAULT 'running'
        CHECK(outcome IN ('running','completed','interrupted','failed')), PRIMARY KEY(workspace,owner_id,id)
);
CREATE TABLE events (
    conversation_id uuid NOT NULL REFERENCES conversations ON DELETE CASCADE, sequence bigint NOT NULL,
    object_id uuid NOT NULL UNIQUE, PRIMARY KEY(conversation_id,sequence)
);
-- Register objects before upload. A crash or SQL rollback leaves a durable garbage collection entry.
CREATE TABLE object_cleanup (object_id uuid PRIMARY KEY, due_at timestamptz NOT NULL);
CREATE FUNCTION enqueue_deleted_event() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO object_cleanup VALUES(OLD.object_id,now()) ON CONFLICT DO NOTHING;
    RETURN OLD;
END $$;
CREATE TRIGGER deleted_event AFTER DELETE ON events FOR EACH ROW EXECUTE FUNCTION enqueue_deleted_event();
CREATE VIEW project_json AS SELECT p.*, to_jsonb(p)-'workspace'-'owner_id' AS document FROM projects p;
CREATE VIEW conversation_json AS SELECT c.*, p.workspace, p.owner_id, p.harness_id, p.root, p.path,
    (to_jsonb(c)-'active_run'-'lease_until'-'bot_connection'-'from_bot') || jsonb_build_object('harness_id',p.harness_id) AS document
    FROM conversations c JOIN projects p ON p.id=c.project_id;
CREATE TABLE approvals (
    id uuid PRIMARY KEY, conversation_id uuid NOT NULL REFERENCES conversations ON DELETE CASCADE, run_id uuid NOT NULL,
    operation text NOT NULL, fingerprint text NOT NULL, state text NOT NULL DEFAULT 'pending'
        CHECK(state IN ('pending','allowed','denied','expired'))
);
CREATE TABLE project_grants (
    project_id uuid NOT NULL REFERENCES projects ON DELETE CASCADE, operation text NOT NULL, fingerprint text NOT NULL,
    PRIMARY KEY(project_id,operation,fingerprint)
);
CREATE TABLE erased_subjects (workspace uuid NOT NULL,owner_id uuid NOT NULL,PRIMARY KEY(workspace,owner_id));
CREATE TABLE stored_objects (object_id uuid PRIMARY KEY,workspace uuid NOT NULL,owner_id uuid NOT NULL);
CREATE INDEX stored_object_subject ON stored_objects(workspace,owner_id);
CREATE FUNCTION protect_subject() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_advisory_xact_lock(hashtextextended(NEW.workspace::text || ':' || NEW.owner_id::text, 0));
    IF EXISTS(SELECT FROM erased_subjects WHERE workspace=NEW.workspace AND owner_id=NEW.owner_id) THEN
        RAISE EXCEPTION 'Subject erased';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER protect_project BEFORE INSERT ON projects FOR EACH ROW EXECUTE FUNCTION protect_subject();
CREATE TRIGGER protect_staging BEFORE INSERT ON stored_objects FOR EACH ROW EXECUTE FUNCTION protect_subject();

CREATE TABLE connections (
    id uuid PRIMARY KEY, workspace uuid NOT NULL, created_by uuid, kind text NOT NULL CHECK(kind IN ('slack','telegram','gitlab')),
    destination text NOT NULL,project_id uuid REFERENCES projects ON DELETE SET NULL,
    status text NOT NULL DEFAULT 'pending', identity text NOT NULL DEFAULT '',external_id text,hook_id text,
    state_hash text, state_until timestamptz, oauth_user uuid,
    enabled boolean NOT NULL DEFAULT false, revision bigint NOT NULL DEFAULT 1, instructions text NOT NULL DEFAULT '',
    triggers jsonb NOT NULL DEFAULT '{"mentions":true,"direct_messages":false}',
    tools jsonb NOT NULL DEFAULT '{"read":"auto","write":"review","execute":"review","mcp":"review"}',
    rate_limit_per_hour integer NOT NULL DEFAULT 30
);
CREATE UNIQUE INDEX connection_destination ON connections(kind,external_id,destination) WHERE status<>'disconnected' AND external_id IS NOT NULL;
CREATE UNIQUE INDEX telegram_webhook_owner ON connections(kind,external_id) WHERE kind='telegram' AND status<>'disconnected' AND external_id IS NOT NULL;
CREATE TABLE external_links (
    connection_id uuid NOT NULL REFERENCES connections ON DELETE CASCADE,external_id text NOT NULL,user_id uuid NOT NULL,
    control_grant uuid NOT NULL,PRIMARY KEY(connection_id,external_id),UNIQUE(connection_id,user_id)
);
CREATE TABLE link_challenges (
    code_hash text PRIMARY KEY,connection_id uuid NOT NULL REFERENCES connections ON DELETE CASCADE,external_id text NOT NULL,
    user_id uuid NOT NULL,control_grant uuid NOT NULL,expires_at timestamptz NOT NULL
);
CREATE TABLE mentions (
    id uuid PRIMARY KEY,connection_id uuid NOT NULL REFERENCES connections ON DELETE CASCADE,delivery_id text NOT NULL,
    owner_id uuid NOT NULL,address jsonb NOT NULL,body_object uuid NOT NULL REFERENCES stored_objects,
    reply_object uuid REFERENCES stored_objects,state text NOT NULL, run_id uuid, conversation_id uuid REFERENCES conversations ON DELETE SET NULL,
    created_at timestamptz NOT NULL DEFAULT now(), replied_at timestamptz, remote_id text,
    UNIQUE(connection_id,delivery_id)
);
CREATE FUNCTION enqueue_deleted_mention() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO object_cleanup VALUES(OLD.body_object,now()) ON CONFLICT DO NOTHING;
    IF OLD.reply_object IS NOT NULL THEN INSERT INTO object_cleanup VALUES(OLD.reply_object,now()) ON CONFLICT DO NOTHING; END IF;
    RETURN OLD;
END $$;
CREATE TRIGGER cleanup_mention AFTER DELETE ON mentions FOR EACH ROW EXECUTE FUNCTION enqueue_deleted_mention();
ALTER TABLE mentions ADD COLUMN lease_until timestamptz, ADD COLUMN lease_id uuid;
ALTER TABLE conversations ADD COLUMN bot_connection uuid REFERENCES connections ON DELETE SET NULL;
-- Deleted connectors must not silently turn their conversations into interactive tool grants.
ALTER TABLE conversations ADD COLUMN from_bot boolean NOT NULL DEFAULT false;
CREATE TABLE link_notices (connection_id uuid NOT NULL REFERENCES connections ON DELETE CASCADE,delivery_id text NOT NULL,PRIMARY KEY(connection_id,delivery_id));

CREATE FUNCTION protect_connector_subject() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE tenant uuid; actor uuid;
BEGIN
    SELECT workspace INTO STRICT tenant FROM connections WHERE id=NEW.connection_id;
    actor := (to_jsonb(NEW)->>TG_ARGV[0])::uuid;
    PERFORM pg_advisory_xact_lock(hashtextextended(tenant::text || ':' || actor::text, 0));
    IF EXISTS(SELECT FROM erased_subjects WHERE workspace=tenant AND owner_id=actor) THEN
        RAISE EXCEPTION 'Subject erased';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER protect_mention BEFORE INSERT ON mentions FOR EACH ROW EXECUTE FUNCTION protect_connector_subject('owner_id');
CREATE TRIGGER protect_challenge BEFORE INSERT ON link_challenges FOR EACH ROW EXECUTE FUNCTION protect_connector_subject('user_id');
CREATE TRIGGER protect_link BEFORE INSERT ON external_links FOR EACH ROW EXECUTE FUNCTION protect_connector_subject('user_id');

CREATE TABLE interactive_calls (
    id uuid PRIMARY KEY, conversation_id uuid NOT NULL REFERENCES conversations ON DELETE CASCADE,
    run_id uuid NOT NULL
);
