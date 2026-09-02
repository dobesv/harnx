---
harnx: minor
---
Prompt before quitting the TUI while the agent is still working. Ctrl+D, `.exit`, and picker exits now open a confirmation modal when a turn is in flight: Ctrl+D exits without interrupting (work continues or resumes on reopen), Ctrl+C durably interrupts the session and then exits, and Esc stays. The modal copy reflects how the session runs (remote, local worker owned by this client, or owned by another client). Ctrl+C awaits the cancel before shutting down so the interrupt can't be lost to the local worker being torn down; if it fails, the TUI still exits and prints a warning to stderr. Idle exit is unchanged.
