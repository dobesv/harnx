---
harnx: patch
---
Compile each `use_tools` selector once per tool-selection pass instead of once for every selector and tool pair. Workers were rebuilding the same glob regexes on every model request and tool round, which took most of their CPU once many agents and tool servers were registered.
