-- The pending-upload guard is belt-and-braces: UNMARK_STALE_PENDING_WITH_PARENT
-- already cleared `is_stale` on marker-holding rows (and their ancestor dirs)
-- before this runs, but the invariant — a sweep never deletes the only record
-- of an unsent edit — is cheap enough to also enforce where the deleting
-- happens.
DELETE FROM items
WHERE
	parent = ?
	AND is_stale = TRUE
	AND trashed = FALSE
	AND pending_upload_at IS NULL;
