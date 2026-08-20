-- Drops the pending-upload marker from exactly one row, named by uuid.
--
-- The stable-scoped variant deliberately prefers the live half of a duplicate
-- stable pair (same scoping as mark_pending_upload); a caller that has already
-- resolved which row it means — a probe answer, a fresh upsert — must not go
-- back through that preference, or it releases the wrong row's marker.
UPDATE items
SET pending_upload_at = NULL
WHERE items.uuid = ?1;
