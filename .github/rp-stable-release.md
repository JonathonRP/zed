# RP stable release authorization

`JonathonRP/zed` has one owner, so it cannot truthfully require approval from a
second GitHub account. RP stable releases instead separate merge, compatibility
evidence, and publication into auditable owner actions.

## Trust boundaries

The release workflow distinguishes two commits:

- The **control SHA** is the current `release/rp-stable` tip selected for
  `workflow_dispatch`. It supplies the workflow, verifier, and committed
  authorization records.
- The **product SHA** is the historical merge named by `release_sha`. It
  supplies the source and packaging scripts only after authorization succeeds.

The authorization job checks out the control SHA under `control/`, confirms it
is still the live release-branch tip, and executes the verifier from that path.
It does not replace that checkout with historical source. Later jobs start on
clean runners and check out the authorized product SHA explicitly.

Pushes to `release/rp-stable` perform only inexpensive PR/check provenance and
authorization-record validation. Metadata allocation, Windows/Linux builds,
assembly, and publication run only for a manual dispatch. A manual dispatch
with `publish_release=false` performs a complete non-publishing rebuild.
Release runs are never canceled by a newer run, avoiding interruption while an
immutable release is being created.

Windows packaging preserves all `ZED_RP_*` values across Visual Studio developer
shell initialization. That shell imports a line-oriented environment and can
otherwise truncate the JSON manifest and release notes to their first lines.
Before Cargo runs, packaging checks the restored identity and release-notes
SHA-256. Before copying the built client into the bundle, it runs
`zed.exe --verify-rp-release-metadata target/rp-release-metadata` to compare the
actual embedded manifest, source/version identity, and complete notes with the
generated files. This command exits without initializing the app, opening a
profile, or checking for updates.

Run the focused multiline environment and Rust `option_env!` regression with:

```powershell
pwsh -NoProfile -File script\test-rp-release-metadata.ps1
```

The Windows packaging identity check also runs this regression.

## Installed calendar floor

The product source pins an exclusive minimum calendar version in
`.github/rp-release-calendar.json`. Metadata generation requires this file and
allocates each new version above both its floor and all published calendar tags.
The floor is read from the exact product commit using `git show`, never from an
uncommitted working-tree file or a later control commit.
The floor is `20261028.1` because that calendar is already installed from the
unpublished 1.22 bootstrap. Publishing `20261028.1` again would not offer that
installation an update, even if the public release points to a new source.

Before final source validation, record the highest calendar version installed
from any unpublished build in `minimum_exclusive`. Raise it if a later build
was installed; never lower it or create reservation tags to stand in for
installation evidence. Revalidate the resulting immutable source normally.
An existing tag on the same source remains reusable only if its version exceeds
the floor; otherwise generation fails instead of rewriting that identity.
Allocation also fails if the UTC date predates a known calendar identity or the
next patch exceeds the client's 32-bit limit.

Both compatibility and production workflows use this policy through the same
metadata generator. Passing a compatibility build does not publish its calendar
or authorize a release. Do not install a new compatibility calendar after final
source validation without rechecking the public release's exclusive minimum.

## Required evidence

Every release target must be a unique merge on the first-parent history of
`release/rp-stable`. It must be a two-parent merge whose second parent is the
tested PR head, and the merge tree must be byte-identical to that head so base
changes cannot enter after testing. Its exact PR head must have a successful
`Validate RP stable compatibility` job from
`.github/workflows/rp_stable_sync.yml`. The workflow blob is pinned in the
authorization record; ordinary pushes also require the PR not to modify its own
validation workflow. Merge `5b0c427...` is the sole explicit bootstrap
exception because its first parent predates that workflow; it must match the
audited blob `dbdb8538...`. Later releases always require equality with the
first-parent workflow blob, even if their authorization record pins another
blob.

Publication additionally requires a source-keyed JSON record under
`.github/rp-release-authorizations/`. The record keeps the ordinary PR check
separate from copied-profile evidence and binds the owner's runtime/WSL result
to:

- the exact product merge and PR head;
- the copied-profile control commit and workflow blob;
- successful Actions run and job IDs;
- GitHub artifact IDs, names, sizes, and SHA-256 digests;
- the extracted metadata manifest hash and its product/upstream identities.

The verifier compares the record with live GitHub API data and downloads the
small metadata artifact. Missing, expired, renamed, or changed evidence fails
before a product checkout or build. The committed record remains an audit trail
after artifact expiry, but cannot authorize a new publish once live evidence is
unavailable.

The sole owner can ultimately change both code and authorization records.
Removing that residual risk requires a second principal or external signing
authority; self-review cannot provide dual control.

## Adding a future authorization

1. Run the normal PR compatibility job on the final product head.
2. Run copied-profile compatibility against that same immutable head.
3. Complete copied-profile runtime and WSL validation using those exact
   source-keyed artifacts.
4. Add a new authorization record in a later control-only PR. Record the
   workflow commit/blob, run/jobs, artifact digests, metadata hashes, product
   head, merge, upstream tag, and upstream commit.
5. Merge the control-only PR after its normal compatibility check. Its push
   validates provenance but does not build or publish.
6. Dispatch the release from the current `release/rp-stable` ref.

Changing the product PR head after either compatibility run invalidates the
evidence and requires retesting.

## Release `5b0c427ee3a4956527c652ff7ea4c156113be2b1`

Do not rerun failed push run `33935425725`; it contains the obsolete
independent-review gate. After the gate-fix PR is merged, optionally perform a
full dry run:

```sh
gh workflow run fork_stable_release.yml \
  --repo JonathonRP/zed \
  --ref release/rp-stable \
  -f release_sha=5b0c427ee3a4956527c652ff7ea4c156113be2b1 \
  -f validated_pr_head_sha=407ca4bc4ff66ee6f406d2e70fa8ce5976523c22 \
  -f publish_release=false
```

Inspect the authorization summary before changing the final input to
`publish_release=true`. It must report:

- a current control SHA distinct from historical product merge `5b0c427...`;
- PR #15 and ordinary compatibility run/job
  `33908384435` / `101138690327`;
- copied-profile run `33909407320`, control commit `f5a6fdb...`, and workflow
  blob `0d2da4e...`;
- tested head `407ca4bc...`;
- upstream tag `v1.18.0` at `49448afc...`.

After publication, verify the immutable RP tag resolves to `5b0c427...`, the
release manifest records the same product/upstream identity, and `SHA256SUMS`
covers every asset. The release workflow does not install packages, alter live
profiles or processes, or mutate rollback assets.
