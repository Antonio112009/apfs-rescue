# Releasing

Versions follow [Semantic Versioning](https://semver.org/): `MAJOR.MINOR.PATCH`.
Before 1.0, a MINOR bump may change command-line options.

## Steps

1. On `main`, make sure CI is green.
2. In a short pull request:
   - set `version` in `Cargo.toml` (for example `0.2.0`) and run `cargo check`
     so `Cargo.lock` follows;
   - in `CHANGELOG.md`, rename `## [Unreleased]` to `## [0.2.0] - YYYY-MM-DD`
     and add a new empty `## [Unreleased]` above it.
3. Merge it, then tag the merge commit and push the tag:

   ```bash
   git switch main && git pull
   git tag -a v0.2.0 -m "apfs-rescue 0.2.0"
   git push origin v0.2.0
   ```

4. `.github/workflows/release.yml` then:
   - checks that the tag matches `Cargo.toml`;
   - builds macOS (Apple chip and Intel), Windows (x64 and ARM) and Linux
     (x64 and ARM) programs;
   - signs and notarizes the macOS programs if a certificate is configured;
   - records a signed build provenance attestation for every file;
   - publishes a GitHub release with the archives, `SHA256SUMS.txt` and the
     notes for this version from `CHANGELOG.md`. GitHub adds the source code
     archives (zip and tar.gz) to every release automatically.

A tag with a suffix, like `v0.2.0-rc.1`, becomes a pre-release.

## Code signing

**Provenance (free, always on).** Every release file gets a Sigstore-signed
attestation that it was built by this repository's workflow from a given
commit. Users check it with `gh attestation verify FILE --repo OWNER/apfs-rescue`.

**macOS (optional).** Without an Apple Developer ID, macOS blocks the
downloaded program until the user runs `xattr -d com.apple.quarantine`. To
sign and notarize, join the Apple Developer Program, create a *Developer ID
Application* certificate, and add these repository secrets:

| Secret | Value |
|---|---|
| `APPLE_CERTIFICATE_P12` | the certificate exported as .p12, base64-encoded |
| `APPLE_CERTIFICATE_PASSWORD` | password of the .p12 file |
| `APPLE_SIGNING_IDENTITY` | e.g. `Developer ID Application: Name (TEAMID)` |
| `APPLE_ID` | Apple ID email used for notarization |
| `APPLE_APP_PASSWORD` | an app-specific password for that Apple ID |
| `APPLE_TEAM_ID` | the 10-character team ID |

The signing step runs only when `APPLE_CERTIFICATE_P12` is set.

**Windows (not set up).** Unsigned programs trigger a SmartScreen warning.
Options: Azure Trusted Signing, or an OV/EV code-signing certificate from a
certificate authority. Add a signing step to the Windows build when one is
available.
