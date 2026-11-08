# Release security

Release binaries are built only in GitHub Actions. The release workflow uses fresh
GitHub-hosted runners with Debian 13 (trixie) containers pinned by digest,
separate Cargo/Rust/build directories, full commit SHA
references for actions and the committed dependency lockfile. It does not restore
or save build/tool caches. Build jobs use `cargo fetch --locked`, followed by
`cargo build --frozen`; offline Cargo prevents additional Cargo downloads during
compilation, but is not an operating-system network sandbox for build scripts.

Two independent build jobs must produce byte-identical binaries and Debian
packages. CI installs and smoke-tests the generated glibc package in a separate
Debian 13 container. The production feature set is explicitly `ui,metrics`; the
installed package must serve the UI, status, Prometheus metrics and SSE endpoints.
Runtime dependencies are generated using `dpkg-shlibdeps`.
A separate job attests their checksums. A final job uploads every asset
to a draft release before publishing it. The repository enables release
immutability, so future published releases and their assets cannot be replaced.
These controls establish artifact provenance and detect build differences; they
do not prove that reviewed source code or upstream toolchains are harmless.

## Source and publication controls

- `main` requires an independent approval, Core code-owner review, approval after
  the last push, resolved conversations and successful required checks.
- Stale approvals are dismissed. Branch protection applies to administrators.
- Release tags cannot be updated or deleted. Only repository administrators may
  create `v*` or `pre-rel-*` tags.
- The release workflow rejects a tag unless its commit is in `main` and its name
  exactly matches the server package version.
- Actions tokens default to read-only access and cannot approve PRs. Build jobs
  retain read-only permissions. Attestation and release upload have only their
  specific additional permissions and do not check out or run project code.
- Actions must be pinned to full commit SHAs. The issue-to-project workflow using
  a long-lived organization token has been removed.

The JSON files in `.github/security/` record the intended repository settings.
Repository administrators can change these settings; protecting against takeover
of an administrator requires organization-level controls managed by independent,
trusted owners. Review organization audit logs and rotate credentials accessible
to suspect workflow runs as part of incident response.

## Preparing a release

1. Update `server/Cargo.toml`, its workspace dependency version, the corresponding
   workspace entry in `Cargo.lock` and `docs/releases/VERSION.md` in a PR.
2. Let CI test the changes and perform both clean candidate builds. Obtain the
   required independent review and merge into `main`.
3. Create `vVERSION` at the reviewed main commit. The tag starts a new pair of
   clean builds. Publication waits for their reproducibility check and attestation.
4. Verify the published assets against their checksums, provenance and expected
   commit. Do not treat a checksum downloaded with an unverified binary as proof
   of trusted origin.

## Review for the 0.3.1 rebuild

The review examined the local reachable commits attributed to `scx1332` and the
GitHub PR metadata and diffs for #378, #380 and #381 on 2026-11-08.

| Change | Observed character | Included in release source? |
| --- | --- | --- |
| PR #378 | Documentation describing a relay alias-redirection issue; no executable changes | No |
| PR #380 | Virtual TCP source validation, OS RNG seed, and tests; permits proven aliases in addition to the sender's own address | No |
| PR #381 / `4ea185b` | OS RNG seed and virtual TCP integration tests, based on PR #379 | No |
| `0588dd1` (2024) | `protoc-prebuilt` dependency bump from 0.2.0 to 0.3.0 | Historical ancestor; this dependency has since been removed |
| `e199ee1` (2024) | Rust/toolchain and CI changes, formatting, and an added test await | Historical ancestor; current release pipeline replaces the old release workflow |
| `8d544dd` (2023) | Workspace package version bumps and `ya-client-model` dependency update | Historical ancestor |
| `9c10f2b` (2023) | Merge of PR #337: action version bumps and conditional protoc download | Historical ancestor; current protobuf build uses externally installed protoc |

The reviewed recent PR diffs contain no observed extra command execution or
data-exfiltration mechanism. They were not merged. Their absence from the release
history does not establish who pushed other commits, whether release assets were
replaced, when the account was compromised, or whether dependencies were ever
compromised. Git author fields and timestamps are not reliable actor attribution.
The older dependency updates were classified by their diffs, not audited as
complete transitive dependency source trees.

The original `7cbe72a74..0d13956` range changes three files: virtual TCP source
validation, interface RNG seeding, and reconnect test expectations. It changes no
dependencies or CI scripts. The independent source check on the release branch
accepts only the session sender's own IPv6 address; it does not use the broader
alias allowance proposed in PR #380.

Before hardening, Actions tokens defaulted to write access and could approve PRs,
release immutability was disabled, and branch protection allowed admin bypass and
did not name required checks. A snapshot of the 16 Actions cache entries was
saved before deleting them. The caches included PR merge refs for #378, #380,
#381 and #379; their existence is not evidence of cache poisoning.
