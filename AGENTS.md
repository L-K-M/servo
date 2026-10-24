# Combray's downstream Servo

## Upstream: source pulls only

Owner instruction, 23 October 2026: `servo/servo` rejects AI-generated
contributions. Never contact or contribute to that project. Only pull/fetch
source. No issues, PRs, comments, discussions, reviews, reactions, pushes,
GitHub fork API calls or outreach. Pass this rule to every helper.

This repository is an independent downstream copy with upstream Git history.
All changes and reviews belong here or in `L-K-M/Combray`. Upstream contribution
instructions in inherited files do not authorize upstream interaction.

## Maintenance

- Keep the engine changes small, one logical change per commit. Preserve MPL
  notices and the existing licenses. Every engine behavior fix carries a
  regression test (a WPT test under `tests/wpt/mozilla/tests/` or a Rust unit
  test) that fails before the fix and passes after it; documentation, build
  and workflow changes get checks that fit them. Fix the engine generally and
  to spec; no site-specific code, URL or user-agent sniffing.
- Follow upstream releases. The newest `combray/0.N` branch is the published
  downstream branch and the repository's default branch. Merge upstream's
  `release/v0.N` updates into it with merge commits (`--no-ff`). When upstream
  publishes a new release line `v0.M`, create `combray/0.M` from the published
  `combray/0.N` tip, merge the release tag into it with `--no-ff`, and make it
  the default branch once that merge is validated; the old branch stays
  frozen. Never rebase or rewrite published downstream history.
- Fetch upstream by explicit URL, without configuring an upstream push remote:
  `git fetch --no-tags https://github.com/servo/servo.git <ref>`. A single
  upstream fix may be backported with `git cherry-pick -x`; prefer that to
  writing our own.
- `DOWNSTREAM.md` lists every downstream change with its reason, check and
  drop condition. Update it in the same PR as the change.
- After each upstream merge, remove downstream code that upstream now makes
  redundant: keep upstream's implementation and any downstream behavior it
  does not cover, keep tests that still add coverage, verify with the
  regression tests rather than reverting old commits blindly, and list each
  removal in the merge PR and `DOWNSTREAM.md`.
- Reconcile copied `.github/workflows/` before publishing an upstream merge.
  Upstream automation was removed deliberately; it can contact other projects.
- The owner's browsing history is private. Never publish its URLs, hosts,
  titles, page content or anything derived from them that identifies a site
  in commits, branches, PRs, issues, tests, fixtures, logs or other
  artifacts. Describe bugs through synthetic test cases written from scratch.
- `L-K-M/Combray` pins a full downstream commit SHA, never a moving branch.
  Validate a candidate with its native builds and fixture/device checks before
  updating that pin. Record evidence and failures in its `docs/ENGINE_SPIKE.md`.
- Use downstream PRs for subsequent changes, targeting the current
  `combray/0.N`. This repository has no CI. Before merging, require a
  successful local build, the applicable regression and WPT tests, and an
  independent review by a reviewer other than the author (another person or
  another model family), with the results recorded in the PR. Leave the PR
  unmerged when that review is unavailable.

A nightly task on the owner's Mac does this maintenance: it re-checks the sites
the owner visits with Combray's site-check harness, fixes what breaks, follows
upstream releases and moves Combray's pin. See `docs/SITECHECK.md` in
`L-K-M/Combray`.

The app, JNI bridge and validation tooling stay in `L-K-M/Combray`.
