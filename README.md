# Zed

[![Zed](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/zed-industries/zed/main/assets/badge/v0.json)](https://zed.dev)
[![CI](https://github.com/zed-industries/zed/actions/workflows/run_tests.yml/badge.svg)](https://github.com/zed-industries/zed/actions/workflows/run_tests.yml)

Welcome to Zed, a high-performance, multiplayer code editor from the creators of [Atom](https://github.com/atom/atom) and [Tree-sitter](https://github.com/tree-sitter/tree-sitter).

---

### Installation

On macOS, Linux, and Windows you can [download Zed directly](https://zed.dev/download) or install Zed via your local package manager ([macOS](https://zed.dev/docs/installation#macos)/[Linux](https://zed.dev/docs/linux#installing-via-a-package-manager)/[Windows](https://zed.dev/docs/windows#package-managers)).

Other platforms are not yet available:

- Web ([tracking discussion](https://github.com/zed-industries/zed/discussions/26195))

### Developing Zed

- [Building Zed for macOS](./docs/src/development/macos.md)
- [Building Zed for Linux](./docs/src/development/linux.md)
- [Building Zed for Windows](./docs/src/development/windows.md)

### Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md) for ways you can contribute to Zed.

Also... we're hiring! Check out our [jobs](https://zed.dev/jobs) page for open roles.

### RP fork stable-sync controller

In `JonathonRP/zed`, `.github/workflows/rp_stable_sync.yml` runs proposals from
`automation/rp-control`. Both the rebased proposal and manual-port handoff use
SSH for Git operations, with a dedicated `RP_STABLE_SYNC_DEPLOY_KEY` secret.
The key is written to a temporary file only in those trusted write steps,
uses GitHub SSH host keys obtained from the HTTPS metadata API with strict
host-key checking, and is removed when the step exits, including on failure.
Missing credentials or unavailable host keys fail the step; there is no token
fallback. `GITHUB_TOKEN` retains read-only contents access and the existing
PR and validation-dispatch API permissions. Validation does not receive the key.

This authentication change is a candidate, not an approved deployment. Before
enabling it, the repository owner must approve the credential and control repair,
provision a new repository write deploy key dedicated to stable sync, and store
its private half as `RP_STABLE_SYNC_DEPLOY_KEY`. Do not reuse, replace, or expose
`RP_MAIN_SYNC_DEPLOY_KEY`, export a CLI token, broaden token permissions, or bypass
repository rules. Deploy keys grant repository-wide Git write access, not
branch-scoped access; review that authority and keep the secret confined to the
trusted controller's two write steps.

Review the control-only diff and run
`python -m unittest discover -s script -p 'test_rp_stable_sync.py'` before
promotion. The reviewed controller must include both the workflow and
`script/rp_stable_sync_ssh.sh`. Promote it through the existing reviewed-control
process only after owner approval; separately review and authorize any
workflow-blob transition required by the existing release/provenance gates.
This candidate supplies no transition authorization or release change.
After approved credential setup and promotion, verify both write paths and key
cleanup in controlled runs before relying on scheduled operation. Local mocked
tests do not establish that GitHub accepts the key or the workflow-containing
pushes.

The exact live release-tip checks, deterministic branches, existing proposal
force-with-lease, non-forced manual handoff, human-progress preservation, and
provenance/manual-review gates remain in place. Nothing auto-merges or publishes
a release. The daily `0 0 * * *` schedule and main-sync job are unchanged;
the cause of missed scheduled runs is unproven and recurrence is not verified.

### Licensing

Zed source code is licensed primarily under GPL-3.0-or-later, with Apache-2.0 components where marked.

License information for third party dependencies must be correctly provided for CI to pass.

We use [`cargo-about`](https://github.com/EmbarkStudios/cargo-about) to automatically comply with open source licenses. If CI is failing, check the following:

- Is it showing a `no license specified` error for a crate you've created? If so, add `publish = false` under `[package]` in your crate's Cargo.toml.
- Is the error `failed to satisfy license requirements` for a dependency? If so, first determine what license the project has and whether this system is sufficient to comply with this license's requirements. If you're unsure, ask a lawyer. Once you've verified that this system is acceptable add the license's SPDX identifier to the `accepted` array in `script/licenses/zed-licenses.toml`.
- Is `cargo-about` unable to find the license for a dependency? If so, add a clarification field at the end of `script/licenses/zed-licenses.toml`, as specified in the [cargo-about book](https://embarkstudios.github.io/cargo-about/cli/generate/config.html#crate-configuration).

## Sponsorship

Zed is developed by **Zed Industries, Inc.**, a for-profit company.

If you’d like to financially support the project, you can do so via GitHub Sponsors.
Sponsorships go directly to Zed Industries and are used as general company revenue.
There are no perks or entitlements associated with sponsorship.
