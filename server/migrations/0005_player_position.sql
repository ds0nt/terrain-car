-- Last known true-space position per player, so a returning player resumes
-- roughly where they left off instead of always landing at a freshly
-- computed spawn-anchor point (see server/src/persistence.rs and
-- server::auth's login flow). Saved on disconnect from the live
-- `car_sim::PlayerPositions` tracking (itself fed continuously by
-- `PlayerPositionMsg`, sent regardless of car/plane/on-foot).
create table if not exists player_positions (
    player_id uuid primary key references players(id),
    true_x double precision not null,
    true_z double precision not null
);
