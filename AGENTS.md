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
  notices and the existing licenses.
- `combray/0.6` is the published downstream branch. Merge tested upstream
  maintenance revisions into it; do not rewrite published history.
- Fetch upstream by explicit URL, without configuring an upstream push remote:
  `git fetch --no-tags https://github.com/servo/servo.git <ref>`.
- Reconcile copied `.github/workflows/` before publishing an upstream merge.
  Upstream automation was removed deliberately; it can contact other projects.
- `L-K-M/Combray` pins a full downstream commit SHA, never a moving branch.
  Validate a candidate with its native builds and fixture/device checks before
  updating that pin. Record evidence and failures in its `docs/ENGINE_SPIKE.md`.
- Use downstream PRs for subsequent changes, targeting `combray/0.6`.

The app, JNI bridge and validation tooling stay in `L-K-M/Combray`.
