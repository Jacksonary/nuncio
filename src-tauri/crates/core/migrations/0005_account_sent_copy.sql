-- Who saves the Sent copy for this account: 'server', 'client', or NULL for
-- automatic (resolved from the provider directory). Existing accounts keep
-- NULL so their behaviour is unchanged until the user chooses.
ALTER TABLE account ADD COLUMN sent_copy TEXT;
