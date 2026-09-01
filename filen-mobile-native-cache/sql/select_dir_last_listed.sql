-- Every directory's last-listed stamp, for ordering a reconcile pass.
--
-- A pass re-lists thousands of containers and is routinely killed before it
-- finishes, so the ORDER decides what a truncated pass actually accomplished.
-- Oldest first means the containers that have gone longest without a listing
-- are the ones it got to.
--
-- Roots are absent by construction (they are not `dirs` rows), which sorts them
-- first — right, since the account root gates every top-level item.
SELECT
	items.uuid,
	dirs.last_listed
FROM dirs
INNER JOIN items ON dirs.id = items.id;
