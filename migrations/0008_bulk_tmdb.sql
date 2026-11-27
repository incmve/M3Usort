-- TMDB id from the provider's bulk get_series list (movies already had provider_tmdb).
-- Enrichment copies a valid one into tmdb_id, so no per-title lookup is needed.
ALTER TABLE series ADD COLUMN provider_tmdb TEXT;
