---
harnx: patch
---
Fix `plans_get_*` and `plans_list_*` output in `bash` scripts. All output modes print JSON that can be piped to `jq`: a document for getters or an `{items, next_cursor}` page for list tools. Python calls return dictionaries, and `help` shows the available fields. Direct-call formatting and links are unchanged.
