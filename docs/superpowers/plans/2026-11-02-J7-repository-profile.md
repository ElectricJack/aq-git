# J7 Repository Profile Decision Implementation Plan

> **For agentic workers:** Execute this bounded research plan in the assigned AQ worktree. Use superpowers:executing-plans for a subsequent code widening; no implementation is authorized by survey results alone without specifying its failing conformance fixtures here first.

**Goal:** Survey all 22 supervisor-inventoried v1 projects, rank profile widenings by active/paused impact, and deliver either a tested widening or an explicit evidence-based refusal.

**Architecture:** Gather read-only Git and GitHub policy observations at recorded revisions. Distinguish absent repositories, inaccessible evidence, and known blockers; never equate an unknown with support. Keep the existing profile unless the survey justifies a bounded conformance-tested widening.

**Tech Stack:** Git 2.43, git-lfs, GitHub CLI, Python 3 for collecting observations, Markdown review report; Rust only if the decision selects implementation.

**Spec:** Task fleet-glacier-27.4, aq-git specification sections 5.2 and 16, and the parent plan's Global Constraints. Inventory authority: supervisor-provided aq project list export, message msg-55308fda5dff4f9a8081bc108475732b.

## Global Constraints

- Rust uses the committed lockfile with `--locked`.
- Unsupported required capabilities refuse explicitly; nothing silently degrades.
- No changes to `docs/specs/*`, onboarding, releases, downstream pins, or performance.
- No secrets in evidence. Read remote policy without changing it.
- Evidence records exact commands, environment, revision and limits.
- Commit/push through AQ. The final origin/dev merge instruction applies to code tasks; this documentation-only refusal has no origin/dev ref. Submit the report as a review.
- A documented refusal is an explicitly permitted acceptance path, not an unimplemented feature claimed as passing.

## Review Focus

1. Paused projects remain in the 22-project denominator; show active and paused counts separately.
2. Missing paths/API authorization errors remain unknown, not feature absence.
3. Attributes in nested/vendor trees and gitlinks must be included, not only root dotfiles.
4. Remote signing rules may differ from local commit.gpgsign; inspect protected branches and rules separately.
5. A checkout can differ from the remote default branch; record revisions and never claim all-history qualification from one tree.

### Task 1: Capture and classify the inventory

**Files:** temporary read-only survey collector/output under `.aq/tmp/J7/`; committed raw evidence `docs/implementation/J7-survey.json`; report draft `docs/implementation/J7.md`.

**Interfaces:** consumes the 22-row supervisor export; produces one observation per project with status, path, revision, history shape, attributes, gitlinks, LFS, policy and size.

- [x] Capture object format, shallow/partial metadata, tracked `.gitmodules`/gitlinks, every tracked `.gitattributes` body and token, LFS listing and local driver configuration.
- [x] Query GitHub metadata, branch signature protection and applicable rules for default/development branches. Preserve errors and distinguish unsupported checks.
- [x] Measure tracked blob bytes and Git object storage, with a separate unavailable value when a checkout is missing.
- [x] Cross-check vault membership and list extra directories without making them projects.
- [x] Verify exactly 22 unique rows, six active rows, and a fit/reason plus evidence limitations for each.

### Task 2: Make the supported-profile decision

**Files:** `docs/implementation/J7.md`; production files only if explicitly added to this plan after the inventory.

**Interfaces:** consumes survey observations; produces ranked unique-repository impact and either a refusal or a conformance-qualified capability change.

- [x] Rank attribute subsets, submodules, LFS, partial/shallow history, signing and object format by distinct affected active and paused repositories; distinguish removing one blocker from fully unblocking a repository.
- [x] If refusing widening, explain the exact safety/cost tradeoff, retain `no_attributes_v1` and its advertisement, and state the evidence required to revisit. No new runtime tests are needed for an unchanged implementation.
- [x] Conditional widening branch not selected (explicit refusal). If selecting widening, update this plan before code with exact test names and expected failures covering workspace creation/checkpoint, candidate merge, artifact import and unsupported mixed attributes. Run those tests RED, implement the smallest named profile, then run them GREEN and update capability advertisement/documentation.

### Task 3: Verify and submit

**Files:** report draft and this plan; raw evidence retained outside the source checkout when finished.

**Interfaces:** consumes final survey/decision; produces submitted review identity/hash and published task branch.

- [x] Check report completeness against every task requirement and inspect report totals against raw evidence.
- [x] Run `scripts/check-docs` and `git diff --check`. If Rust changes, also run the required format/clippy/test checks with `--locked` where applicable; do not reuse old qualification.
- [x] J7 has no `review: adversarial` marker; submit the required document review without inventing a separate gate.
- [ ] Submit `docs/implementation/J7.md` through `aq review submit --kind other`; retain its canonical review/vault identity and hash. Follow the task's explicit instruction to move the draft outside the source checkout after submission.
- [ ] Publish the assigned branch, settle the AQ checklist, and close with exact evidence, limits and review identity.

## Recorded outcome

Survey complete: 22 members, six active and sixteen paused. Five active projects have no observed profile blocker; aq-server requires whitespace attributes. Overall: 15 supported at the inspected trees, four attribute-blocked, one LFS-blocked, one unverified checkout and one without a repository. The raw JSON records full revisions, all attribute bodies, policy reads and missing evidence.

Selected the permitted explicit refusal: retain `no_attributes_v1` and its existing capability advertisement. A separate whitespace-only profile ranks first (one active project), followed by standard text/EOL/binary support (two paused projects). The former's diagnostics-only demand does not justify expanding the released contract in this pass; the latter needs independent conformance work. Custom merge drivers and LFS remain distinct high-risk requirements. No release, downstream pin, specification edit or runtime change is made.

Validation: `scripts/check-docs` passed, reporting eight cross-repository links unverified because the worktree has no adjacent sibling checkouts; `git diff --check` passed. The structured-evidence audit reconciled all 22 rows, status/fit counts, frozen source-tree entries, feature checks and signing-policy responses. No runtime test qualification is claimed for unchanged Rust code. The report is submitted through AQ Reviews and moved outside the checkout per the assignment's deliverable instructions; this plan and raw evidence remain on the task branch.
