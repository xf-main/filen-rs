SELECT
	items.id,
	items.uuid,
	items.parent,
	items.trashed,
	items.local_data,
	items.type,
	items.change_seq
FROM items
LEFT JOIN dirs_meta ON items.id = dirs_meta.id
LEFT JOIN files_meta ON items.id = files_meta.id
WHERE
	items.parent = ?1
	AND items.trashed = FALSE
	AND (
		?2 = files_meta.name OR ?2 = dirs_meta.name
		OR ?2 = uuid_text(items.uuid)
	)
ORDER BY
	CASE
		WHEN ?2 = files_meta.name OR ?2 = dirs_meta.name THEN 0
		WHEN ?2 = uuid_text(items.uuid) THEN 1
	END,
	-- Most-recently-CHANGED row first within a rank: same-named twins are legal
	-- (an out-of-listing ingest lands a fresh row next to a stale same-named
	-- one; a move can land a live file next to a live same-named sibling), and
	-- the row with the newest replica-visible change is the best liveness
	-- signal either way — a fresh ingest carries the top sequence, and so does
	-- a just-moved row, while rowid order would prefer whichever happened to be
	-- inserted first. `id` only stabilizes the (roots-only) zero-seq tie.
	items.change_seq DESC,
	items.id DESC
LIMIT 1;
