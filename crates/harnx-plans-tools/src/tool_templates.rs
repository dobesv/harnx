//! Call templates the client renders for each plans tool.
//!
//! Both the MCP `list_tools` handler and the native `Toolset` read these, so a
//! tool renders the same way over either transport.

/// Every plans tool answers with one text block, so they share a result template.
pub(crate) const RESULT: &str = "{{ result.content[0].text | default('') }}";

pub(crate) const LIST_PLANS_CALL: &str = "list plans";

pub(crate) const ADD_PLAN_CALL: &str = "create plan {{ args.name }}{% if args.title %} — {{ args.title | truncate(40) }}{% endif %}{% if args.git_branch %} [{{ args.git_branch }}]{% endif %}{% if args.github_owner_repo %} ({{ args.github_owner_repo }}){% endif %}{% if args.content %}
{{ args.content | truncate(80) }}{% elif args.body %}
{{ args.body | truncate(80) }}{% endif %}";

pub(crate) const GET_PLAN_CALL: &str = "read plan {{ args.name }}";

pub(crate) const UPDATE_PLAN_CALL: &str = "update plan {{ args.name }}{% if args.title %} — {{ args.title | truncate(40) }}{% endif %}{% if args.git_branch %} [{{ args.git_branch }}]{% endif %}{% if args.github_owner_repo %} ({{ args.github_owner_repo }}){% endif %}{% if args.tasks %} [{{ args.tasks | length }} tasks]{% endif %}{% if args.content %}
{{ args.content | truncate(80) }}{% elif args.replace_content %}
{{ args.replace_content | truncate(80) }}{% endif %}{% if args.append_content %}
+{{ args.append_content | truncate(80) }}{% endif %}";

pub(crate) const DELETE_PLAN_CALL: &str = "delete plan {{ args.name }}";

pub(crate) const LIST_TASKS_CALL: &str = "list tasks {{ args.plan }}{% if args.filter and args.filter != 'open' %} [{{ args.filter }}]{% endif %}{% if args.tag %} #{{ args.tag }}{% endif %}";

pub(crate) const ADD_TASK_CALL: &str = "create task {{ args.plan }}/{{ args.title }}{% if args.status %} [{{ args.status }}]{% endif %}{% if args.assignee %} @{{ args.assignee }}{% endif %}{% if args.executor %} ▶{{ args.executor }}{% endif %}{% if args.tags %} #{{ args.tags | join(' #') }}{% endif %}{% if args.body %}
{{ args.body | truncate(80) }}{% endif %}";

pub(crate) const GET_TASK_CALL: &str = "read task {{ args.plan }}/{{ args.id }}";

pub(crate) const UPDATE_TASK_CALL: &str = "update task {{ args.plan }}/{{ args.id }}{% if args.title %} — {{ args.title | truncate(40) }}{% endif %}{% if args.status %} [{{ args.status }}]{% endif %}{% if args.assignee %} @{{ args.assignee }}{% endif %}{% if args.executor %} ▶{{ args.executor }}{% endif %}{% if args.tags %} #{{ args.tags | join(' #') }}{% endif %}{% if args.replace_body %}
{{ args.replace_body | truncate(80) }}{% endif %}{% if args.append_body %}
+{{ args.append_body | truncate(80) }}{% endif %}";

pub(crate) const DELETE_TASK_CALL: &str = "delete task {{ args.plan }}/{{ args.id }}";

pub(crate) const LIST_NOTES_CALL: &str = "list notes {{ args.plan }}";

pub(crate) const ADD_NOTE_CALL: &str = "add note {{ args.plan }}{% if args.summary %} — {{ args.summary | truncate(60) }}{% endif %}{% if args.author %} by {{ args.author }}{% endif %}{% if args.body %}
{{ args.body | truncate(80) }}{% endif %}";

pub(crate) const GET_NOTE_CALL: &str = "read note {{ args.plan }}/{{ args.note_id }}";

pub(crate) const UPDATE_NOTE_CALL: &str = "update note {{ args.plan }}/{{ args.note_id }}{% if args.summary %} — {{ args.summary | truncate(60) }}{% endif %}{% if args.replace_body %}
{{ args.replace_body | truncate(80) }}{% endif %}{% if args.append_body %}
+{{ args.append_body | truncate(80) }}{% endif %}";

pub(crate) const DELETE_NOTE_CALL: &str = "delete note {{ args.plan }}/{{ args.note_id }}";
