# Debian packaging

Crumb has two separate Debian distribution tracks. They must not be described
as the same thing.

## Elixpo APT repository

Protected Crumb releases build `amd64` and `arm64` `.deb` files alongside the
portable archives. The packages are intended for Elixpo's signed repository at
`https://packages.elixpo.com/apt/`. After that repository publishes a stable
Crumb package, users can install it through their normal APT workflow.

This makes Crumb an **Elixpo-provided Debian package**. It does not mean that
Crumb has been accepted into the official Debian archive.

## Official Debian archive

Debian rebuilds applications from source without fetching Cargo dependencies
during the normal archive build. An audit on 19 October 2026 found 104 unique
packages in the Linux dependency graph for `crumb-cli`:

- 61 external crates already represented in Debian unstable;
- 29 internal Crumb workspace crates; and
- 14 external crates not represented in Debian unstable.

The missing external crates were:

```text
cms 0.2.3
keyring-core 1.0.0
reedline 0.52.1
serde_json_canonicalizer 0.4.1
sigstore-bundle 0.15.0
sigstore-crypto 0.15.0
sigstore-merkle 0.15.0
sigstore-rekor 0.15.0
sigstore-trust-root 0.15.0
sigstore-tsa 0.15.0
sigstore-tuf 0.15.0
sigstore-types 0.15.0
sigstore-verify 0.15.0
zbus-secret-service-keyring-store 1.0.1
```

The first official-archive action is therefore a design review with the Debian
Rust Team, not an upload of the binary package. The review must settle whether
Crumb's internal workspace crates can remain one application source package and
how the missing Sigstore and keyring dependency families should enter Debian.

## Debian Rust Team request

Send the following to `debian-rust@lists.debian.org` from the identity that will
maintain the Debian package. Subscribe to the list or explicitly watch for
replies before sending it.

```text
Subject: Advice on packaging Crumb, a Rust workspace application, for Debian

Hello Debian Rust Team,

I maintain Crumb, an MIT-licensed self-hosted agent harness and
natural-language terminal:

  https://github.com/elixpo/crumb.elixpo

I would like to maintain it in Debian and file an ITP after confirming the
correct source-package structure. Crumb builds one user-facing binary,
`crumb`, from a workspace containing 29 internal crates. A `cargo debstatus`
audit for the Linux `crumb-cli` graph found 61 external crates in unstable and
14 missing external crates. Most of the missing set is the Sigstore 0.15
family; the others are cms, keyring-core, reedline,
serde_json_canonicalizer, and zbus-secret-service-keyring-store.

Would the team recommend keeping the unpublished internal workspace crates in
one application source package, or introducing them as separate Rust source
packages? For the missing Sigstore family, should I coordinate their packaging
under the Rust Team before filing Crumb's ITP?

Crumb declares rust-version 1.95, which is available in current Debian
development suites. I am prepared to maintain the package and its missing
dependencies and to test a fully offline build.

Thank you,
Ayushman Bhattacharya
Elixpo
https://crumb.elixpo.com
```

## Archive submission sequence

1. Obtain the Rust Team's direction on the workspace and missing crates.
2. Search WNPP for an existing Crumb effort.
3. File `ITP: crumb -- self-hosted agent harness and natural-language terminal`
   against `wnpp` using `reportbug`.
4. Prepare and sign the policy-compliant Debian source package.
5. Build it offline and run `lintian`, `autopkgtest`, `piuparts`, and `sbuild`.
6. Upload the signed source package to `mentors.debian.net`.
7. File an RFS against `sponsorship-requests` and respond to sponsor review.

Do not announce Crumb as being in Debian until the package clears Debian's NEW
queue and appears in the official archive.
