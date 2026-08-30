# Marketplace and memory boundaries

Crumb's marketplace is an offline-first package catalog. The binary ships a public catalog,
and the website publishes the same metadata and artifacts at `/marketplace/catalog.json`.
Adding a marketplace never happens during terminal startup.

## Package lifecycle

1. A namespaced package (`publisher/name`) declares a version, license, source, capabilities,
   artifacts, and one or more skill or MCP entries.
2. Crumb rejects unknown fields, unsafe paths, duplicate identifiers, oversized files, and
   malformed environment-variable references.
3. Installation copies only declared regular files into an immutable project or user cache,
   verifies every SHA-256 digest, and atomically publishes the version.
4. Installation registers components disabled. It does not run hooks, start MCP processes,
   resolve secret values, or grant permissions.
5. A later explicit user action may load a skill. External MCP execution will remain disabled
   until Crumb's MCP client/proxy owns discovery, approval, cancellation, output filtering, and
   per-call audit events. Passing marketplace MCPs directly into a provider CLI would let that
   provider bypass Crumb's permission broker, so this release intentionally refuses that shortcut.

The schema lives at `marketplace/schema/catalog.schema.json`. The initial public catalog contains
two inspectable skills and one descriptor for Crumb's built-in policy-controlled workspace MCP.

## Memory lifecycle

Durable memory has two scopes:

- project: `<workspace>/.crumb/MEMORY.md`
- user: `~/.crumb/memory/MEMORY.md`

Only `/memory remember <project|user> <text>` writes an entry. Models have no durable-memory write
tool. Entries are single-line, bounded, atomically saved, and rejected when they resemble common
credentials. Agent turns receive a bounded read-only projection labeled as user-approved context,
never as permission. `/memory show`, `/memory forget`, `/memory compact`, and `/memory status` keep
the state inspectable and reversible.

Session state stays in the existing session journal and bounded recent-command context. It is not
silently promoted into durable memory. This prevents transient tool output, provider payloads, and
secrets from becoming long-lived preferences.

## Optimizer coverage

Normal shell output is printed unchanged. When recent command output is later attached to an agent
request, Crumb first redacts and bounds it, then lazily invokes configured optimizers such as RTK.
An external result is accepted only when it is smaller and preserves every critical diagnostic.
Optimizer absence, timeout, or failure falls back to the native filtered result.

## Competitive interoperability decisions

The manifest follows the strongest common conventions rather than cloning one product:
namespaced IDs and metadata-only registry entries from MCP Registry, versioned cached packages and
separate install/enable phases from established CLI marketplaces, and portable `SKILL.md` content.
Crumb adds digest verification, explicit capability declarations, no lifecycle scripts in schema
v1, secret references instead of values, and project/user install scopes.

Before general third-party MCP activation, Crumb still needs a policy-owning MCP client/proxy,
server identity pinning, tool-schema snapshots, per-call approvals, cancellation, health status,
and bounded optimized outputs. Those are release blockers, not optional polish.
