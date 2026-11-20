-- Microsoft Graph reports built-in task lists through `wellknownListName`.
-- `defaultList` (Tasks) and `flaggedEmails` (Flagged Emails) cannot be renamed
-- or deleted, so the list row has to remember that it is not user-created.
ALTER TABLE task_list ADD COLUMN well_known_list_name TEXT NOT NULL DEFAULT 'none';
