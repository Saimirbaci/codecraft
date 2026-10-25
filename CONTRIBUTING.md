# Contributing to CodeCraft

Thanks for helping. CodeCraft is an MIT-licensed, Rust-only IDE that reimplements the *behaviour*
and workflow of JetBrains RustRover. `AGENTS.md` is the technical guide (architecture, golden
rules, what to run before you finish); this page covers what we need from every contributor.

## The clean-room rule

CodeCraft must never contain JetBrains' code or assets. Read `docs/clean-room.md`; the short
version:

**You may**

- Use RustRover as a user, and describe what it does, how its menus, shortcuts and tool windows
  behave and look.
- Use JetBrains' public documentation for feature descriptions.
- Consult permissively licensed sources (IntelliJ Community, Apache-2.0; the archived
  intellij-rust plugin, MIT) for behaviour. If you adapt anything from them, add it to
  `ATTRIBUTION.md` in the same PR.

**You must not**

- Decompile, disassemble or extract anything from RustRover or other JetBrains binaries or JARs.
- Copy JetBrains code, icons, fonts, colour schemes, inspection descriptions, templates, keymap
  or action XML, or any other file or text from their products.
- Paste text from RustRover's UI (tooltips, long descriptions, inspection messages) into the code
  or docs. Menu labels and shortcuts are recorded in `crates/engine/src/catalog.rs`; write
  anything longer in your own words.
- Contribute if you have had access to JetBrains' proprietary source code and would be
  reproducing it from memory.

If you are unsure whether something is allowed, ask in the issue or PR before writing it.

## Your contribution is yours to give

By opening a PR you confirm that:

1. You wrote it (or it comes from a source whose licence allows MIT redistribution, named in the
   PR and in `ATTRIBUTION.md`), and you have the right to submit it.
2. It follows the clean-room rule above.
3. You license it under the project's [MIT licence](LICENSE).

Sign off every commit to say so (the [Developer Certificate of Origin](https://developercertificate.org/)):

```sh
git commit -s -m "Describe the change"   # adds: Signed-off-by: Your Name <you@example.com>
```

## Branding

CodeCraft is not affiliated with or endorsed by JetBrains. Don't add JetBrains or RustRover
names, logos or look-alike icons to the app, installers or docs, other than factual comparisons
("compatible with RustRover's default keymap"). Icons and artwork must be original or under a
licence that allows MIT redistribution, with the source recorded in `ATTRIBUTION.md`.

## Making a change

1. Open an issue first for anything larger than a small fix.
2. Follow `AGENTS.md`: every action is a command with an id from `catalog.rs`, the UI stays thin,
   nothing blocks the UI thread, and non-test code never panics (no `unwrap`/`expect`/`panic!`,
   no `unsafe`).
3. Add tests. A crash fix needs a regression test that failed before. Tests must not need the
   network and must skip cleanly when `cargo` or `rust-analyzer` are missing.
4. Before you push:

   ```sh
   cargo test -p <crates you touched>
   cargo clippy -p <crates> --all-targets -- -D warnings
   cargo xtask layers
   cargo xtask parity    # if you added commands; commit the regenerated docs/parity-checklist.md
   ```

   Don't run `cargo update`; `unicode-ident` is pinned on purpose (see `AGENTS.md`).
5. Keep PRs focused, and say in the description what you changed and how you checked it.

## Using AI coding agents

Agent-written code is held to the same rules, and the person submitting it is responsible for
them: review it, run the checks above, and make sure it contains nothing copied from JetBrains
products or from code whose licence you haven't checked.

## Reporting a problem

Bugs and ideas go in GitHub issues. If you believe something in the repo copies JetBrains'
material or breaks the clean-room rule, open an issue or contact the maintainers privately; we
will review it and remove it promptly if it is a problem.
