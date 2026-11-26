-- Persistent revision counter for `message_flag_writer.seq`.
--
-- The revision is compared by readers (auto mark-as-read during a body load)
-- to detect that a flag field changed after they observed it. Deriving the next
-- value with `MAX(seq) + 1` reuses a number once every writer row carrying it
-- was deleted: undoing a "mark unread" with no predecessor deletes its writer,
-- so a second "mark unread" could get the exact revision the first undo's
-- reader had already seen and silently overwrite it.
--
-- The counter row is independent of `message_flag_writer`, so deleting writers
-- never moves it backwards. Existing databases start above every revision they
-- have handed out so far.
CREATE TABLE IF NOT EXISTS flag_writer_seq (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    value INTEGER NOT NULL
);

INSERT INTO flag_writer_seq (id, value)
SELECT 1, COALESCE((SELECT MAX(seq) FROM message_flag_writer), 0)
WHERE NOT EXISTS (SELECT 1 FROM flag_writer_seq);
