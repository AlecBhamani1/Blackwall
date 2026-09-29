# Development and production workflow

`partial` is the default, long-lived integration branch. `main` is production.
Every change enters through a pull request; a release batches the work accumulated on `partial`.

```mermaid
flowchart LR
  I[Issue in version milestone] --> F[Feature branch from partial]
  F --> C[Pull request and CI]
  C --> P[partial]
  P --> R[Release notes, version, acceptance, milestone]
  R --> G[partial to main PR and both macOS builds]
  G --> M[main merge commit]
  M --> V[Passing production CI]
  V --> B[Signed builds and artifact verification]
  B --> S[Versioned release and updater feed]
  S --> Y[main to partial sync PR]
  Y --> P
```

## Start and track a change

1. Create a bug or feature issue. Assign it to the intended `v<version>` milestone and apply
   relevant type, priority, or dependency labels. The milestone is the release batch's task list.
2. Branch from the latest integration branch:

   ```sh
   git fetch origin
   git switch -c feat/my-change origin/partial
   ```

3. Make a focused change and use Conventional Commits. Update the appropriate changelog section
   for user-visible behavior. Run the checks described in [CONTRIBUTING.md](../CONTRIBUTING.md).
4. Open a PR targeting `partial`. Link the issue with `Closes #123`, include verification and any
   UI screenshots, and assign the same milestone. Because `partial` is the default branch,
   merging the development PR closes linked issues and updates the milestone's progress.
5. CI requires Rust formatting/lint/tests, Linux relay tests and container acceptance,
   dependency policy, dependency vulnerability review, UI check/test/build, and release-policy
   tests. `Quality gate` fails when an applicable job fails, is cancelled, or unexpectedly skips.
   `Production promotion` checks the branch contract. Both statuses are required.
6. Resolve review conversations and update the branch to the latest base if needed. Squash merge
   development PRs with a Conventional Commit title. Delete the feature branch after merging.

Both long-lived branches prohibit direct pushes, force pushes, deletion, and administrator bypass.
The repository currently has one collaborator, so mandatory external reviews are set to zero.
Code ownership routes reviews to the maintainer. Once another reviewer joins, run
`npm run repo:configure -- --apply --reviews=1` to require fresh approval and code-owner review.

## Promote a release batch

1. Check the version milestone: intended issues are complete and deferred work is explicitly
   moved to a later milestone. Finish applicable [native acceptance checks](NATIVE_ACCEPTANCE.md).
   Record evidence and remaining limitations accurately; automated builds cannot establish
   physical-device acceptance or Apple signing/notarization.
2. Start a release-preparation branch from `origin/partial` and run:

   ```sh
   npm run release:prepare -- 0.1.4
   ```

   This updates `.github/release.json` and creates `docs/releases/0.1.4.md` using the existing
   release format: `# Blackwall <version>`, `Changes`, `Downloads`, and `Acceptance status`.
   Fill every placeholder, link the milestone URL, and update `CHANGELOG.md`.
   Merge the preparation PR into `partial` through the normal checks.
3. Ensure the previous production version and its compatibility feed have finished publishing.
   Open a PR with head `partial` and base `main`, title `chore(release): publish v0.1.4`, label
   `release`, and milestone `v0.1.4`. Complete the three production checklist items from the PR
   template. Checking the publication item and merging authorizes automatic publication.
4. The production gate requires this repository's `partial` branch, a newer stable version,
   complete notes, the matching milestone, explicit checked acceptance, and the latest `main`
   commit in `partial`'s history. Both Apple Silicon and Intel desktop builds must also pass.
   Merge this PR with a **merge commit**. Production rules prohibit squash/rebase promotions,
   which would sever shared history and make the next batch difficult to compare.
5. The main push runs CI. Its successful completion automatically starts the signed release
   workflow for that exact commit. All installers, updater archives, signatures, and manifest
   entries must pass verification before the version is published as Latest. The original
   `main/latest.json` compatibility feed advances afterward. See [UPDATES.md](UPDATES.md).
6. Verify the downloads and updater. Close the version milestone once publication and release
   verification are complete, then create the next milestone. Release notes and the release
   page record the source commit; the milestone records the included issues and PRs.
7. Bring the production merge commit back into integration through a sync PR:

   ```sh
   gh pr create --base partial --head main --title 'chore: sync production into partial' \
     --body 'Preserve production merge ancestry before the next release batch.'
   ```

   After CI passes, merge that PR with **Create a merge commit**. Never squash this sync PR,
   delete `partial`, or reset it. Development can continue while the signed build runs, but a
   subsequent production promotion must wait for the preceding version and feed to publish.

## Urgent fixes, failed releases, and recovery

For an urgent fix, pause unrelated merges into `partial`, land a focused fix through its checked
PR, and promote a patch version through the same process. This batching model promotes everything
already in `partial`; keep unfinished functionality behind safe defaults or feature flags.

A failed build leaves a draft and preserves the existing updater feed. Re-run failed jobs.
If publication succeeded but the compatibility feed failed, retry the publish job or manually
dispatch **Publish versioned release** from `main` with acceptance confirmed. A full retry now
skips builds of an already-published version and verifies/repairs its feed. Never replace published
files or move a version tag. For a bad application release, revert the affected change through a
PR into `partial` and publish a **new, higher patch version**; the updater refuses downgrades.

## GitHub setup and bootstrap

Repository policy is reproducible and dry-run by default:

```sh
npm run repo:configure
npm run repo:configure -- --apply
```

The script creates `partial` at `main` if absent, installs explicit branch rules without bypasses,
sets `partial` as default, configures merge methods, adds tracking labels, and enables secret
scanning/push protection, Dependabot security fixes, and private vulnerability reporting.
Version tags matching `v*` are protected against updates and deletion.
Existing unrelated rulesets remain in place. Weekly dependency PRs target `partial` and undergo
the same CI gates. Feature branches are deleted explicitly so a release merge cannot delete
the long-lived integration branch.

The setup PR first lands in `partial`; it does not publish an application version. The committed
release baseline is the existing `0.1.3` release. The first promotion chooses `0.1.4` or higher
and installs this automation on `main`. The production gate reads the existing versioned notes
as its baseline during that one-time migration. No acceptance checklist is pre-approved.
