-- Remote images now load by default (matching the Outlook and 163 web clients).
-- Rows written before this migration carry the old "ask" default; only rewrite
-- that exact value so an explicit user choice of "trustedOnly" is preserved.
UPDATE settings
SET json = json_set(json, '$.mail.remoteImages', 'always')
WHERE json_extract(json, '$.mail.remoteImages') = 'ask'
  AND json_valid(json);
