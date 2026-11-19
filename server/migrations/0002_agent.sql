-- Content is stored only in encrypted transcript objects. These identifiers fence replies.
ALTER TABLE conversations
    ADD COLUMN goal_id uuid,
    ADD COLUMN question_id uuid,
    ADD COLUMN plan_id uuid;
