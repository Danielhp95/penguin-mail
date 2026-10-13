# Security

Penguin Mail holds OAuth tokens for Gmail accounts and renders mail from
strangers, so a flaw can expose someone's mail.

Report a vulnerability privately through
[GitHub's security advisories](https://github.com/c9dev/penguin-mail/security/advisories/new),
not in a public issue. Include the version, the steps that show the
problem, and what an attacker gains.

Only the latest release gets fixes.

## What we check on a pull request

A mail client holds passwords and tokens, so a pull request gets more
than a code review. Expect these:

- `cargo deny check` against [deny.toml](deny.toml). Every crate must come
  from crates.io under a licence that combines with GPL-3.0-or-later, with
  no known vulnerability and no yanked release. A git dependency fails.
- GitHub's dependency review, which fails on a new dependency with a high
  or critical advisory.
- [zizmor](https://docs.zizmor.sh) over every workflow. Actions are pinned
  by commit, checkouts keep no token, and no workflow builds pull request
  code with secrets in reach.
- A list of what to read closely, on the run's summary page: build scripts,
  crates new to `Cargo.lock` and which are proc macros, added `unsafe`,
  added binary files, and long or base64-looking lines. A binary file
  outside `docs/`, `app/data/` or `testmail/` fails the check.
- The `security-review` label and a review from the code owner, for
  changes to workflows, scripts, packaging, manifests, build scripts, the
  app's data files, and the code that signs in or keeps secrets. See
  [.github/CODEOWNERS](.github/CODEOWNERS).

Keep a pull request to one purpose. Explain each new dependency: what it
does for the change and why the standard library or a crate already in
`Cargo.lock` does not do it. We close a pull request that hides a change
inside a large reformat, a regenerated file or a binary.
