---
harnx: patch
---
Tool results that harnx prints to a pipe or file are no longer cut to the width of the terminal it was started from, or to 80 columns when `TERM` is set without a terminal; they get the same limits as when no terminal is attached.
