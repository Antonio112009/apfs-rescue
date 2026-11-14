# Contributing

## Workflow: trunk-based development

- `main` is the trunk. It must always build, pass CI and be releasable.
- Work in a short-lived branch (hours or a few days), named after the change:
  `fix/omap-lookup`, `feat/windows-long-paths`, `docs/guide-photos`.
- Open a pull request into `main` early. CI (`.github/workflows/ci.yml`) runs
  formatting, clippy with warnings as errors, tests on Linux, macOS and Windows,
  and a build with the minimum supported Rust version.
- Merge with **squash merge** once CI is green, so `main` gets one commit per change.
- No long-lived `develop` or release branches. Releases are tags on `main`
  (see [RELEASING.md](RELEASING.md)).
- Unfinished features that must land early go behind a hidden command-line flag
  rather than a long-lived branch.

Recommended GitHub settings for `main`: require a pull request, require the
CI checks to pass, allow squash merging only, and delete branches after merge.

## Commit messages

Use [Conventional Commits](https://www.conventionalcommits.org/):
`fix: …`, `feat: …`, `docs: …`, `ci: …`, `refactor: …`, `test: …`.
They make the changelog easy to write.

## Before you open a pull request

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

Add an entry under `## [Unreleased]` in [CHANGELOG.md](CHANGELOG.md) for
anything a user would notice.

## Clean-room rule

apfs-rescue is written from Apple's public *Apple File System Reference* and
from observing real disk images. **Do not copy or translate code from other
APFS implementations**, in particular GPL-licensed ones. Running another tool
as a black box to compare outputs is fine.

## Testing against real images

Unit tests cover parsing helpers. Behaviour on real disks is checked by
extracting from images and comparing SHA-256 checksums with an independent
APFS reader. Never commit disk images or file lists
from them: they contain personal data.
