# bloxtree-cli

Command-line interface to a [bloxtree](../bloxtree-core) content-addressable store.

## Usage

All commands operate on a store root directory, given with `--store <PATH>`
(or the `BLOXTREE_STORE` env var) and defaulting to XDG data home `/bloxtree` if neither is given.
The root must already exist, but in case of the xdg home the cli will ask you if you want to create
it;
`bloxtree-cli` will create `objects/` and `index.db` inside it on first use (open behavior of bloxtree-core.) 

```
bloxtree-cli [GLOBAL OPTIONS] <COMMAND> [COMMAND OPTIONS]
```

### Global options

| Option | Env | Default | Description |
|--------|-----|---------|-------------|
| `--store <PATH>` | `BLOXTREE_STORE` | `$XDG_DATA_HOME/bloxtree` | Path to the bloxtree store root |
| `-h, --help` | — | — | Print help |

## Commands

### `add` — store a blob under one or more paths

```
bloxtree-cli add <PATH>... [--file <FILE>]
```

Reads from `<FILE>` if given, otherwise from stdin. Creates a new version of
each `<PATH>`. Prints the resulting hash and `uuid`.

- `--file <FILE>` — read content from this file instead of stdin

Example:
```
echo "hello" | bloxtree-cli add notes/greeting.txt notes/hello.txt
bloxtree-cli add images/photo.jpg images/copy.jpg --file ./photo.jpg
```

### `get` — fetch the latest version of a path

```
bloxtree-cli get <PATH> [--out <FILE>]
```

Writes the blob bytes to `<FILE>` if given, otherwise to stdout. Exits non-zero
if the path does not exist.

- `--out <FILE>` — write to this file instead of stdout
- `--hash <HASH>` — retrieve a specific version by BLAKE3 hash (default: latest)

### `remove` — delete a path or a specific version

```
bloxtree-cli remove <PATH> [--hash <HASH>]
```

Without `--hash`, removes **all versions** of `PATH` and deletes any blob whose
ref count reaches zero. With `--hash`, removes only that version.

- `--hash <HASH>` — remove only the version with this BLAKE3 hash

### `trim` — keep only the newest N versions

```
bloxtree-cli trim <PATH> <MAX_VERSIONS>
```

`MAX_VERSIONS=0` is equivalent to `remove`. Silent no-op if `PATH` is missing.

### `list` — one-level listing under a prefix

```
bloxtree-cli list [PREFIX]
```

Lists immediate children of `PREFIX` (empty prefix = top level). Prints one
entry per line: `d <prefix>/` for common prefixes, `f <path> <hash>` for leaf
paths.

### `paths` — list all paths

```
bloxtree-cli paths
```

Prints every path in the store with its latest version's hash and `uuid`,
one per line.

### `versions` — show version history of a path

```
bloxtree-cli versions <PATH>
```

Prints versions newest-first. Each line: `<version> <hash> <uuid>`,
where `<version>` is `1` for the latest, `2` for the previous, etc. Empty
output if the path does not exist.

### `info` — show store metadata

```
bloxtree-cli info
```

Prints the store root, on-disk layout, and counts (number of paths, number of
versions, number of blobs). Useful for quick inspection.

## Exit codes

- `0` — success
- `1` — runtime error (path not found, IO failure, hash mismatch, invalid path)
- `2` — usage error (bad arguments)
