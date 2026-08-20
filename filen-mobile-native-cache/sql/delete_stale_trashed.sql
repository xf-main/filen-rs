-- The pending-upload guard mirrors delete_stale_with_parent.sql:
-- UNMARK_STALE_PENDING_TRASHED already cleared `is_stale` on marker-holding
-- rows (and the trashed dirs sheltering one), but the invariant — a sweep never
-- deletes the only record of an unsent edit — is cheap enough to also enforce
-- where the deleting happens.
DELETE FROM items
WHERE
	trashed = TRUE
	AND is_stale = TRUE
	AND pending_upload_at IS NULL;
