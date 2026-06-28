-- The deployment-wide settings table is no longer used. Drop it so upgrades
-- remove any legacy records while retaining the immutable migration history.
DROP TABLE instance_settings;
