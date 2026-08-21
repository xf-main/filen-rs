WITH positions AS (
	SELECT
		value AS uuid,
		key AS position
	FROM JSON_EACH(?)
)

-- `items.uuid` is a 16-byte BLOB; the JSON carries hyphenated UUID text, so
-- strip the hyphens and decode to bytes before matching.
SELECT p.position
FROM positions AS p
LEFT JOIN items AS i ON UNHEX(REPLACE(p.uuid, '-', '')) = i.uuid
WHERE
	i.uuid IS NULL
	-- A uuid the database no longer knows can still name the slot holding the
	-- only copy of an unsent edit: a remote edit re-mints the row's uuid in
	-- place, and the bytes stay under the OLD uuid until the rescue moves them.
	-- Those retired uuids are exactly `slot_remints.old_uuid` (written by the
	-- remint trigger, drained by `rescue_reminted_slots`), so spare them here —
	-- deleting one is the data loss the log exists to prevent.
	AND UNHEX(REPLACE(p.uuid, '-', '')) NOT IN (
		SELECT slot_remints.old_uuid FROM slot_remints
	);
