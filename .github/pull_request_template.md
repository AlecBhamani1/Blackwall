## Changes

Describe the user impact and link the issues addressed (for example, `Closes #123`).
Target `partial` for development. Use a merge commit for a `partial` → `main` release.

## Verification

List checks performed and include screenshots for visible UI changes.
Describe security, data migration, rollout, and recovery considerations where applicable.

## Production release checklist

Complete this section only for a `partial` → `main` promotion. Assign the PR to the
`v<version>` milestone, update `.github/release.json`, and complete the matching release notes.
Merging the promotion authorizes automatic publication of the version and updater feed.

- [ ] Release notes describe this batch and link its milestone.
- [ ] CI and applicable native acceptance checks are complete; evidence and remaining limitations are recorded in the release notes.
- [ ] I approve publishing this version and advancing the signed updater feed.
