-- Ramps (and potentially future building kinds) need an orientation —
-- captured from the placing car's yaw, see PlaceBuildingMsg/BuildingSnapshot.
alter table buildings add column if not exists rotation_y double precision not null default 0;
