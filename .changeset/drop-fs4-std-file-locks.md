---
harnx: patch
---
Drop the `fs4` dependency and lock files through `std::fs::File` instead.

`File::try_lock` and `File::unlock` were stabilised in Rust 1.89, and the inherent methods shadow `fs4`'s extension trait, so the crate was already being bypassed at every call site. Contention and I/O errors now arrive as one `TryLockError`, and a small helper keeps them apart: another process holding the lock makes this one a follower, while an I/O error has to surface rather than be read as a lost election.
