# Publishing @cognelis/mongodb-cli

The public npm package is a small launcher. Native binaries are built on matching
GitHub-hosted runners and downloaded from the public GitHub Release for the exact
package version. The Rust workspace, Cargo lockfile entries, plugin manifests,
and npm package share one plain release version.

## One-time setup

1. Use the public `cognelis/mongodb-cli` GitHub repository with Actions enabled.
   `build/npm/package.json` uses `git+https://github.com/cognelis/mongodb-cli.git`.
   Its owner/repository casing must match `GITHUB_REPOSITORY` for provenance.
2. The publishing npm account must be a member with publish rights in `cognelis`.
   Verify with `npm whoami` and `npm org ls cognelis`. Authenticate using `npm login`;
   never commit registry credentials or copy a token into a release script.
3. The package is public (`publishConfig.access` and `npm publish --access public`).
   First publish creates the package. Do not publish a nonfunctional placeholder.

## First release

1. Follow `CONTRIBUTING.md` to finish changes, write Unreleased notes, prepare the
   version, run tests, and commit the reviewed source on main.
2. Create an annotated `vX.Y.Z` tag on that commit and push main plus the tag.
   `.github/workflows/release.yml` validates the source and builds all six native
   assets. Each runner checks the actual binary version and structured errors and
   tests an npm tarball with that binary before uploading it.
3. The workflow creates a draft GitHub Release, uploads every binary and
   `checksums.txt`, then makes the completed release public. Existing public
   release assets are preserved on reruns; only draft assets may be replaced.
4. Inspect the npm tarball (`npm pack --dry-run --json` in `build/npm`). Then run
   `npm --prefix build/npm run release:publish` using the authenticated npm owner.
   The prepublish check requires valid checksum entries and reachable downloads
   for every declared platform. A registry lookup failure other than E404 stops
   publication; an already published exact version is skipped.
5. Install the published package in a fresh temporary directory **with install
   scripts enabled**, run its launcher, and confirm the actual version, JSON
   output, completion script generation, and failure exit codes.

The initial tag workflow does not attempt npm OIDC publication unless explicitly
enabled. This avoids a circular dependency on trusted publishing before the npm
package exists. If npm requests a browser or 2FA confirmation, finish that step
in the publishing account's session and retry the same verified version.

## Enable automatic npm publishing after the first publish

In npmjs.com, open `@cognelis/mongodb-cli` → Settings → Trusted Publisher and add
GitHub Actions with these fields:

| Field | Value |
| --- | --- |
| Organization or user | `cognelis` |
| Repository | `mongodb-cli` |
| Workflow filename | `release.yml` |
| Environment | Leave blank |
| Allowed actions | Allow direct `npm publish` |

Then add the GitHub repository Actions variable `NPM_TRUSTED_PUBLISHING=true`.
Future version tags publish automatically after successful binary release checks.
npm expires a trusted publisher configuration that has not completed a publish
within 48 hours of its creation, so create it shortly before the release that will
use it; an expired configuration must be deleted and recreated.
The manual release workflow can also publish when `publish_npm` is selected.
The workflow uses Node 24 and checks for npm >=11.5.1, grants `id-token: write`
only to the publishing job, and uses no long-lived npm token or `registry-url`
credential configuration. See the official [npm trusted publishing guide](https://docs.npmjs.com/trusted-publishers/).

## Artifact contract

These filenames must agree with `build/npm/install.js` and
`scripts/build-release.py`:

```text
mongodb-cli-darwin-amd64
mongodb-cli-darwin-arm64
mongodb-cli-linux-amd64
mongodb-cli-linux-arm64
mongodb-cli-windows-amd64.exe
mongodb-cli-windows-arm64.exe
checksums.txt
LICENSE
```

Linux builds use Ubuntu 22.04's glibc baseline and require the `libdbus-1` runtime.
Persistent keyring access also requires a Secret Service session; environment-key
automation does not use a credential store. Alpine/musl is not a prebuilt target.

Do not change a published version's source tag or binary assets to repair a bug.
Prepare a new version. Rerunning an unchanged failed release is supported.
