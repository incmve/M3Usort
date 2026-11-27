-- Where each TMDB id came from. Only real ids are used from now on, never a
-- guess from a TMDB name search:
--   'list'       provider's bulk list (get_vod_streams / get_series)
--   'provider'   provider's get_vod_info / get_series_info
--   'jellyfin'   Jellyfin's match for an item in the library (ProviderIds.Tmdb)
--   'unverified' existing id of unknown origin; the provider job asks again
ALTER TABLE movies ADD COLUMN tmdb_source TEXT;
ALTER TABLE series ADD COLUMN tmdb_source TEXT;

-- Classify ids stored by earlier versions, which also searched TMDB by name.

-- 1. Same as the bulk list: real.
UPDATE movies SET tmdb_source = 'list'
 WHERE tmdb_id IS NOT NULL AND tmdb_id != '' AND tmdb_id = trim(provider_tmdb);
UPDATE series SET tmdb_source = 'list'
 WHERE tmdb_id IS NOT NULL AND tmdb_id != '' AND tmdb_id = trim(provider_tmdb);

-- 2. Provider asked and no TMDB lookup since: the id can only be the provider's.
UPDATE movies SET tmdb_source = 'provider'
 WHERE tmdb_source IS NULL AND tmdb_id IS NOT NULL AND tmdb_id != ''
   AND provider_checked IS NOT NULL AND tmdb_checked IS NULL;
UPDATE series SET tmdb_source = 'provider'
 WHERE tmdb_source IS NULL AND tmdb_id IS NOT NULL AND tmdb_id != ''
   AND provider_checked IS NOT NULL AND tmdb_checked IS NULL;

-- 3. Provider asked and TMDB looked up too: the id is the provider's or a name
--    search after the provider had none. Can't tell: ask the provider again
--    (data stays until then).
UPDATE movies SET tmdb_source = 'unverified', provider_checked = NULL
 WHERE tmdb_source IS NULL AND tmdb_id IS NOT NULL AND tmdb_id != '' AND provider_checked IS NOT NULL;
UPDATE series SET tmdb_source = 'unverified', provider_checked = NULL
 WHERE tmdb_source IS NULL AND tmdb_id IS NOT NULL AND tmdb_id != '' AND provider_checked IS NOT NULL;

-- 4. Provider never asked: the id came from a name search. Remove it and
--    everything TMDB filled in for it (plot comes back from the provider:
--    movies via get_vod_info, series at the next catalog refresh).
UPDATE movies SET tmdb_id = NULL, imdb_id = NULL, poster = NULL, plot = NULL, tmdb_checked = NULL
 WHERE tmdb_source IS NULL AND tmdb_id IS NOT NULL;
UPDATE series SET tmdb_id = NULL, imdb_id = NULL, poster = NULL, plot = NULL, seasons = NULL,
       status = NULL, tmdb_checked = NULL
 WHERE tmdb_source IS NULL AND tmdb_id IS NOT NULL;

-- Searches that found nothing were marked as checked; without an id there is
-- nothing to check.
UPDATE movies SET tmdb_checked = NULL WHERE tmdb_id IS NULL;
UPDATE series SET tmdb_checked = NULL WHERE tmdb_id IS NULL;

DELETE FROM cast_members
 WHERE (kind = 'movie' AND item_id IN (SELECT stream_id FROM movies WHERE tmdb_id IS NULL))
    OR (kind = 'series' AND item_id IN (SELECT series_id FROM series WHERE tmdb_id IS NULL));
