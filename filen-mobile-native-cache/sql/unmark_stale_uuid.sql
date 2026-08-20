-- Spares one row from the stale sweep it is caught in: the caller asked the
-- server what became of a row missing from the fresh listing and could not get
-- a definitive answer, so it is kept this round rather than destructively
-- guessed about (deleting it would cascade and tombstone its whole subtree).
UPDATE items
SET is_stale = FALSE
WHERE uuid = ?;
