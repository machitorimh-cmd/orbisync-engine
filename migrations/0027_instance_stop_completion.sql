-- Bound the stop-completion scan independently of historical instance count.
CREATE INDEX world_instances_stopping_id_idx
    ON world_instances (id) WHERE lifecycle = 'stopping';
