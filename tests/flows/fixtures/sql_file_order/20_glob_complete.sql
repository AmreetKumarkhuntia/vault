-- Runs after 10_glob_started.sql because glob matches are path-sorted.
UPDATE orders
SET status = 'fixture-glob-complete'
WHERE id = 7101 AND status = 'fixture-glob-started';
