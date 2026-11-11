# Contributing

Thank you for your interest. The project is at an early stage: before a large change, please
open an issue to discuss it.

## Developer Certificate of Origin

Every commit must be signed off, certifying that you wrote it or have the right to submit it
under the project's license ([Developer Certificate of Origin](https://developercertificate.org/)):

    git commit -s

This adds a `Signed-off-by: Your Name <you@example.com>` line with your real name. A check on
every pull request refuses commits without it. No contributor license agreement is needed.

## License of contributions

Contributions are accepted under the Mozilla Public License 2.0, the project's license. New source
files start with the SPDX identifier:

```rust
// SPDX-License-Identifier: MPL-2.0
```

## Commits and pull requests

- Write code comments, commit messages and pull request descriptions in English.
- One topic per pull request; describe why the change is needed, what it does, and how it was
  tested.
- Keep `cargo fmt`, `cargo clippy` and `cargo test` clean; continuous integration runs them.

## AI-assisted contributions

AI-assisted contributions are welcome. You remain responsible for understanding, reviewing and
testing everything you submit. Say so in the pull request description, and credit the assistant
with a `Co-authored-by:` trailer if you wish.

## Reporting security issues

Do not open a public issue for a vulnerability: see [SECURITY.md](SECURITY.md).
