-- Spares rows the stale sweep must not delete: a file whose local edit has not
-- reached the server, and every directory sheltering one below it. Deleting the
-- file destroys the pending-upload marker — the only record linking those bytes
-- to an upload obligation, and the only thing the launch drain scans — while
-- deleting an ancestor directory cascades down to the same result.
-- `forget_item` refuses the equivalent single-item deletion for the same
-- reason; this keeps the bulk sweep honest to that invariant. Spared rows stay
-- behind as phantoms until the drain sends the edit and a later listing
-- reconciles them — the accepted trade, because the other side is silent data
-- loss.
--
-- The walk runs UP from every marker through the parent chain (markers are
-- rare, listings are big). UNION rather than UNION ALL: a parent chain that
-- loops back on itself is not something this cache can rule out, and
-- deduplicating the frontier is what makes such a chain terminate.
--
-- Each climb step is taken only when the LOWER row is untrashed, because that
-- is the only edge a delete cascades through (both cascade triggers leave
-- trashed children alone) — the same gate `unmark_stale_pending_trashed.sql`
-- carries, for the same reason. Climbing past a trashed intermediate spares an
-- ancestor whose deletion could never have reached the marker, and since the
-- reconcile probe leaves a permanently-deleted directory to this sweep, that
-- ancestor is then re-spared and re-probed on every refresh, forever.
UPDATE items
SET is_stale = FALSE
WHERE
	parent = ?
	AND is_stale = TRUE
	AND uuid IN (
		WITH RECURSIVE pending_holders (uuid, parent, cascade_reached) AS (
			SELECT
				i.uuid,
				i.parent,
				TRUE
			FROM items AS i
			WHERE i.pending_upload_at IS NOT NULL AND i.trashed = FALSE
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
