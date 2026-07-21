# Contributing to peko-tools

This repository holds the Pekoscript toolchain: the compiler front end
(`peko_core`), the LLVM code generator (`peko_llvm`), the CLI (`peko_cli`), the
language server (`peko_lsp`), and the runtime assets under `toolkit/`.

Contributions are welcome. This document covers what to expect and what a change
needs to pass before it can be merged.

## Licensing of contributions

By opening a pull request you agree that your contribution is licensed under the
MIT License, the same terms as the rest of the project (see [LICENSE](LICENSE)).
You retain copyright in your work; you are granting the same rights to everyone
that the project grants.

Only contribute code you have the right to contribute. Do not paste code from
another project unless its license permits it, and if you do, say so in the pull
request so the attribution can be handled properly (see
[Third-party code](#third-party-code)).

## Before you start

For anything beyond a small fix, open an issue first and describe the problem.
That avoids work being duplicated or rejected for reasons that were not visible
from the outside — the compiler in particular has design constraints that are not
always obvious from the code.

Good first contributions: bug fixes with a reproducing case, diagnostics that
could be clearer, standard library gaps, editor/LSP improvements, documentation
of existing behaviour.

Please open an issue before: changes to the language itself, changes to the
package format or manifest schema, new dependencies, and anything that changes
generated code or the ABI.

## Development setup

You need a recent stable Rust toolchain and LLVM 18.1.x. The code generator
links libLLVM through llvm-sys, so LLVM 18 must be present at build time and
found through an environment variable. See
[Prerequisites](README.md#prerequisites) for the two ways to provide it.

```bash
cargo build            # build the workspace
cargo test --all       # run the test suite
```

## Required checks

Run these before pushing. CI runs the same ones, and a pull request that fails
any of them cannot be merged.

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
```

Clippy is enforced with `-D warnings`: warnings fail the build. Fix them rather
than adding a blanket `allow`. A targeted `#[allow(...)]` with a comment
explaining why is fine when the lint is genuinely wrong.

If you change dependencies, regenerate the third-party notices:

```bash
./scripts/gen-notices.sh
```

CI verifies `THIRD-PARTY-NOTICES.txt` is current and fails if it is stale.

## Commit style

Commit messages are short imperative one-liners describing what the commit does:

```
Route native tooling through a self-contained LLVM 18 bundle
Enable the webview inspector in the dev loop
```

Keep the history readable: one logical change per commit, and rebase rather than
merge when updating a branch.

## Pull requests

- Describe what the change does and why. If it fixes an issue, link it.
- Include a test when the change is testable. Compiler changes especially: a
  test that fails before the fix and passes after is the fastest path to review.
- Keep unrelated changes out. Formatting churn in files you did not otherwise
  touch makes a diff hard to read.
- Expect review comments on compiler internals. The type system, the erasure
  model, and the GC interact in ways that are easy to break silently.

## Third-party code

The toolchain vendors native C/C++ code (BearSSL, webview, the Android native
app glue) and links hundreds of Rust crates. Attribution for all of it must
travel with the binaries we ship, so:

- **Never remove or alter a copyright or license header** in a vendored file. If
  you modify vendored code, keep the original notice and add a note describing
  the modification.
- **Adding a Rust dependency**: run `./scripts/gen-notices.sh` and commit the
  regenerated `THIRD-PARTY-NOTICES.txt`. If the dependency's license is not in
  the accepted list in `about.toml`, raise it in the pull request rather than
  adding it to the list yourself.
- **Adding vendored native code**: add an entry to `NOTICE-native.txt`, and if
  the code is linked into applications built with Peko (rather than only into
  the toolchain), also add it to `NOTICE-app.txt`. A test in
  `crates/peko-cli/src/bundler/mod.rs` pins the app notices and will fail if a
  known component is dropped.

Copyleft-licensed dependencies (GPL, AGPL) will not be accepted. Weak or
file-level copyleft (MPL-2.0) is acceptable for unmodified upstream crates.

## Reporting bugs

Open an issue with the smallest program that reproduces the problem, the exact
command you ran, the output you got, and your platform and `peko --version`.
Compiler bugs without a reproducing case are usually not actionable.

## Security

Do not report security issues in a public issue. Email
[connect@pekoui.com](mailto:connect@pekoui.com) with the details and give us a
chance to ship a fix before disclosing.

## Conduct

Be straightforward and civil. Critique code, not people. Maintainers may close
or block on conduct grounds.
