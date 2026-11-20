-- Never edit this file: databases in the field have already run it.
-- Version 7 stores each thread's report format, the JSON Schema object its agent's final reply
-- must match, or NULL for none.
  ALTER TABLE threads ADD COLUMN report_format TEXT CHECK (report_format IS NULL OR json_valid(report_format));
