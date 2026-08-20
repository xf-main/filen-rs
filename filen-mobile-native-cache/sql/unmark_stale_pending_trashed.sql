-- The trash sweep's counterpart of unmark_stale_pending_with_parent.sql:
-- spares trashed rows the sweep must not delete — a trashed file whose local
-- edit has not reached the server (a remote trash lands via the upsert, which
-- never touches pending_upload_at, so the marker rides into the trash), and a
-- trashed directory sheltering such an edit below it (its deletion would
-- cascade to the marker). The tombstone trigger's own pending guard would
-- suppress the tombstone too, so without this the only record of the unsent
-- edit vanishes with no replica ever told.
--
-- The walk runs UP from every marker through the parent chain; trashed rows
-- keep their original parent in the parent column, so the chain crosses the
-- trash boundary. UNION deduplicates a chain that loops back on itself.
--
-- Each climb step is taken only when the LOWER row is untrashed, because that
-- is the only edge a delete cascades through (cascade_on_delete_delete_children
-- leaves trashed children alone): a marker on a TRASHED row spares that row
-- itself, but its ancestors can be deleted without ever reaching it — climbing
-- anyway left a permanently-deleted dir spared forever on account of an
-- independently-trashed descendant it no longer shelters.
UPDATE items
SET is_stale = FALSE
WHERE
	trashed = TRUE
	AND is_stale = TRUE
	AND uuid IN (
		WITH RECURSIVE pending_holders (uuid, parent, cascade_reached) AS (
			SELECT
				i.uuid,
				i.parent,
				i.trashed = FALSE
			FROM items AS i
			WHERE i.pending_upload_at IS NOT NULL
			UNION
			SELECT
				i.uuid,
				i.parent,
				i.trashed = FALSE
			FROM items AS i
			INNER JOIN pending_holders AS p ON i.uuid = p.parent
			WHERE p.cascade_reached
		)

		SELECT uuid FROM pending_holders
	);
