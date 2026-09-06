-- The actual surface height a building was placed on (server-authoritative
-- raycast at placement time, not raw terrain height) — see
-- BuildingSnapshot::ground_y's docs for why this must be persisted rather
-- than re-derived on load.
alter table buildings add column if not exists ground_y double precision not null default 0;
