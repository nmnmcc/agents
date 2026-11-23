# Contributing

## Development environment

The environment comes from [devenv](https://devenv.sh) only. Run `devenv shell`
(or let direnv load it) to get the Rust toolchain, test services and release
tools, and `devenv test` to run the full test suite.

## Changesets

Changes are recorded the way [Changesets](https://github.com/changesets/changesets)
does it, using [knope](https://knope.tech). Every pull request that changes
user-facing behaviour adds a change file:

```sh
knope document-change
```

This asks for the change type and a summary, then writes
`.changeset/<summary>.md`:

```md
---
default: minor
---

# Short summary for the changelog

Optional details, examples or migration notes.
```

| Type    | Use for                     | Bump while 0.x | Bump from 1.0 |
| ------- | --------------------------- | -------------- | ------------- |
| `major` | Breaking changes            | 0.1.0 → 0.2.0  | 1.0.0 → 2.0.0 |
| `minor` | New features                | 0.1.0 → 0.1.1  | 1.0.0 → 1.1.0 |
| `patch` | Bug fixes and other changes | 0.1.0 → 0.1.1  | 1.0.0 → 1.0.1 |

Edit the file freely before merging. Internal changes such as refactors, tests
and CI need no change file.

## Releases

Releases are automated by `.github/workflows/release.yml`:

1. When main has change files, CI keeps a **Release x.y.z** pull request up to
   date on the `release` branch. It bumps the version in `Cargo.toml` and
   `Cargo.lock`, adds the entry to `CHANGELOG.md` and deletes the change files.
2. Merging that pull request publishes the release. CI builds the static Linux
   (x86_64, aarch64) and macOS (aarch64) binaries, tags `vx.y.z` and creates the
   GitHub release with the changelog entry and the binaries.

Nobody edits versions, tags or `CHANGELOG.md` by hand.
