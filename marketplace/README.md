# Crumb public marketplace

The catalog contains versioned Crumb packages. A package may expose one or more skills,
MCP server launch descriptors, or both. Catalog metadata never grants permissions and
installation never starts a process.

Package identifiers are namespaced (`publisher/name`). Every copied artifact is listed
with a SHA-256 digest and must stay inside its package directory. MCP environment entries
are variable names only; secret values do not belong in manifests, logs, or memory.

## Contributing a package

1. Add a self-contained directory under `packages/<name>`.
2. Put each skill in `skills/<skill-id>/SKILL.md` with concise YAML frontmatter.
3. Add a namespaced, versioned catalog entry and the SHA-256 digest of every artifact.
4. Declare the minimum capabilities needed. Do not include credentials or install hooks.
5. Run `cargo test -p crumb-marketplace` and the workspace merge gate.

The first schema deliberately excludes arbitrary lifecycle scripts, implicit dependencies,
and secret values. Future remote sources will be fetched only by an explicit command,
verified into an immutable cache, and enabled separately by the user.

Validate catalog structure against `schema/catalog.schema.json`. Crumb's Rust validator remains
authoritative and additionally enforces cross-field rules such as unique package identifiers and
skill paths referencing declared artifacts.
