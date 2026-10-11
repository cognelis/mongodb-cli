# Repository workflow

- Follow [CONTRIBUTING.md](CONTRIBUTING.md) for version selection, changelog entries,
  validation, and local installation refreshes. Do not use superpowers skills.
- For user-visible CLI, client API, or bundled Skill changes, add an entry under
  `CHANGELOG.md`'s `Unreleased` section as part of the work. Include migration notes
  for incompatible behavior or authentication changes.
- Before delivering a completed user-visible change batch, select the appropriate
  patch/minor/major increment and run `python3 scripts/release.py prepare <increment>`
  once. If the user explicitly wants an unreleased draft, keep it in Unreleased.
- Keep the workspace version, both lockfile package versions, the npm package, and both plugin
  versions synchronized. Use a normal product version increment; a timestamp
  cachebuster does not count as a release. Do not increment the JSON schema version
  unless its compatibility contract actually changes.
- Run the relevant behavior tests and the release/integration checks before
  reporting completion. Version or formatting checks do not replace regression tests.
- For npm distribution changes, run the Node installer tests and the tarball
  installation smoke test. Do not publish until all declared native release assets
  and their checksums are publicly available. Follow `docs/releasing.md`.
- Keep README and Skill instructions consistent with the implemented CLI. When
  refreshing installed Skills, rebuild the CLI first and verify the installed content.
- Commit, tag, push, and publish only within the scope the user requested. In
  particular, a version bump by itself does not authorize publishing to a remote.
