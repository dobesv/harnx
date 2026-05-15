## Jira lookup with `acli`

When you need Jira issue context, use `acli` directly from the shell. Typical commands:
- Search: `acli jira workitem search --jql "text ~ 'keyword'"`
- View key fields: `acli jira workitem view FDEV-1234 --fields "summary,description,comment,status,assignee"`
- View everything: `acli jira workitem view FDEV-1234 --fields "*all"`

Summarize the useful findings instead of pasting raw output.
