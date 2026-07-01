---
harnx: patch
---
Fix a crash when scrolling the transcript after compaction. The scrolling widget's per-width height cache assumed the number of items only ever grows; when compaction shrank or blanked the transcript, an internal length calculation underflowed — panicking in debug builds and, in release builds, wrapping into an unbounded allocation that could exhaust memory. The cache now resizes to the current item count.
