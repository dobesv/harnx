---
harnx: patch
---
Return interrupted A2A tasks from blocking sends and close SSE responses at InputRequired or AuthRequired without treating the task as terminal. Add a task-local response cursor for waits after an approval reply.
