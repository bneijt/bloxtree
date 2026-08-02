Bloxtree
======

_Simply store everything_

Bloxtree is a content addressable storage with versioning, allowing you to store any blob without worrying about duplicates.

On disk it's a collection of content files with an index database.

This project has multiple packages:
- bloxtree-core: a Rust library that allows you to do all basic operations: add blobs, remove blobs, list, etc.
- bloxtree-cli: a commandline interface to a bloxtree.
- bloxtree-fuse: a FUSE driver that allows you to mount a boxtree and see the latest version for each path.
