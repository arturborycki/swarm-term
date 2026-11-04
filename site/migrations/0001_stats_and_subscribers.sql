-- Daily totals, one row per day, kind, version and country. No IP addresses
-- or per-visitor rows: worker/index.ts only adds 1 to a total.
--   kind: 'download' (the site's Download buttons), 'update' (an installed
--   Swarm downloading an update), 'check' (an installed Swarm reading the
--   update feed, roughly once per launch and per day: a measure of use).
CREATE TABLE stats (
  day TEXT NOT NULL,
  kind TEXT NOT NULL,
  version TEXT NOT NULL,
  country TEXT NOT NULL,
  count INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (day, kind, version, country)
) WITHOUT ROWID;

-- People who asked for release news (the optional form on the site).
CREATE TABLE subscribers (
  email TEXT PRIMARY KEY COLLATE NOCASE,
  created_at TEXT NOT NULL,
  source TEXT NOT NULL,
  unsubscribed_at TEXT
);
