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
  notices and the existing licenses. Every fix carries a regression test (a WPT
  test under `tests/wpt/mozilla/tests/` or a Rust unit test) that fails
  without it. Fix the engine generally and to spec; no site-specific code,
  URL or user-agent sniffing.
- Follow upstream releases. The newest `combray/0.N` branch is the published
  downstream branch and the repository's default branch. Merge upstream's
  `release/v0.N` backports into it. When upstream publishes a new release line
  `v0.M`, start `combray/0.M` at the current tip and merge the release tag
  there; the old branch stays frozen. Merge, never rebase, and do not rewrite
  published history.
- Fetch upstream by explicit URL, without configuring an upstream push remote:
  `git fetch --no-tags https://github.com/servo/servo.git <ref>`. Prefer
  backporting an upstream fix (`git cherry-pick -x`) to writing our own.
- After each upstream merge, drop downstream changes that upstream now covers
  (keep their tests when they still add coverage) and say so in the merge PR.
- Reconcile copied `.github/workflows/` before publishing an upstream merge.
  Upstream automation was removed deliberately; it can contact other projects.
- Never name websites from the owner's browsing history (URLs, hosts, titles,
  page content) in commits, branches, PRs or issues. Describe bugs through
  reduced test cases written from scratch.
- `L-K-M/Combray` pins a full downstream commit SHA, never a moving branch.
  Validate a candidate with its native builds and fixture/device checks before
  updating that pin. Record evidence and failures in its `docs/ENGINE_SPIKE.md`.
- Use downstream PRs for subsequent changes, targeting the current
  `combray/0.N`. This repository has no CI: a local build, the regression test,
  the affected WPT directories and an independent review gate each merge.

A nightly task on the owner's Mac does this maintenance: it re-checks the sites
the owner visits with Combray's site-check harness, fixes what breaks, follows
upstream releases and moves Combray's pin. See `docs/SITECHECK.md` in
`L-K-M/Combray`.

The app, JNI bridge and validation tooling stay in `L-K-M/Combray`.
