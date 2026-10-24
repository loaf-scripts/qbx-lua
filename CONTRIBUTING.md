# Contributing

`qbx-lua` is the Qbox Lua tooling workspace, formerly hosted as `qbx-lint`.

This repository accepts changes to both `qbx-lint` and `qbx-lua-ls`, including their shared
parser, formatter, analysis and FiveM data. The former standalone server repository is archived;
use this repository for language-server issues and pull requests. Editor adapters remain in
[qbx-editor](https://github.com/Qbox-project/qbx-editor).

Use a stable Rust toolchain with `rustfmt` and `clippy`. From the repository root:

```sh
cargo build --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Run the CLI against a resource checkout with:

```sh
cargo run --locked -p qbx_lint -- path/to/resource
cargo run --locked -p qbx_lint -- fmt --check path/to/resource
```

## Where changes belong

| Crate | Purpose |
| --- | --- |
| `qbx_lua_syntax` | Lua and CfxLua lexer, parser, syntax tree, and visitors. |
| `qbx_lua_fmt` | Formatter and output verification. |
| `qbx_luacats` | LuaCATS annotation and type parser. |
| `qbx_fivem_data` | Native data, runtime stubs, and known manifest imports. |
| `qbx_lua_analysis` | Scopes, resource models, rules, configuration, and suppressions. |
| `qbx_lint` | Command-line interface and report formats. |
| `qbx_lua_ls` | Language server, editor features, and LSP protocol. |
| `xtask` | Native data generation. |

The [language server](crates/qbx_lua_ls) shares the workspace's parser, formatter, analysis and
FiveM data crates. `cargo test --workspace --locked` checks both tools; shared API changes belong
in the same pull request. See its [contribution guide](crates/qbx_lua_ls/CONTRIBUTING.md) for LSP
checks and manual probes. Editor adapters live in [qbx-editor](https://github.com/Qbox-project/qbx-editor).

Release builds use separate Cargo invocations so the linter does not enable the server's `docs`
features. The server uses the `release` profile with panic unwinding for request recovery;
the CLI keeps its abort-on-panic behavior in `lint-release`:

```sh
cargo build --profile lint-release --locked -p qbx_lint
cargo build --release --locked -p qbx_lua_ls
```

## Tests and fixtures

Include a small reproduction when changing parser behavior, a lint rule, or an automatic edit.
For a rule change, cover both code that should be reported and a similar valid case. CLI
regressions belong in `crates/qbx_lint/tests`; analysis tests and fixture resources are under
`crates/qbx_lua_analysis/tests`.

Fixture snapshots can be regenerated after an intentional diagnostic change. In a POSIX shell:

```sh
QBX_BLESS=1 cargo test --locked -p qbx_lua_analysis --test fixtures
```

In PowerShell:

```powershell
$env:QBX_BLESS = '1'
cargo test --locked -p qbx_lua_analysis --test fixtures
Remove-Item Env:QBX_BLESS
```

Review the snapshot diff before accepting it, then rerun tests without `QBX_BLESS`.

## Generated data

Native signatures and documentation are checked in. To refresh them from the configured FiveM
data endpoints:

```sh
cargo xtask natives
```

The generator also reads [NativeDB](https://github.com/alloc8or/gta5-nativedb-data) for the
parameters that a game native takes beyond those FiveM declares, which `natives.tsv` writes
`+name`. It takes only their names and types, never NativeDB's comments.

The generator rejects a download that would reduce the native count by more than 10%. Inspect
the data diff even when generation succeeds. The [natives workflow](.github/workflows/natives.yml)
also proposes updates.

Control and ped configuration flag references are generated separately from official Cfx
documentation. Refresh them with `cargo xtask references`, or reproduce the recorded source
revisions with `cargo xtask references --pinned`. The same weekly workflow proposes updates.
See [game reference data](docs/game-references.md) for sources, provenance and validation.
These tables are included only with the `qbx_fivem_data` crate's `docs` feature; check that
path with `cargo test --locked -p qbx_fivem_data --features docs`.

The GLM stub generator requires Node.js with built-in `fetch`. It accepts either a local binding
source directory or a base URL, and uses the configured CfxLua source URL when omitted:

```sh
node scripts/generate-glm-stub.mjs
node scripts/generate-glm-stub.mjs path/to/glm-binding
```

Data generation makes network requests unless local sources are supplied where supported;
ordinary tests use the checked-in data.

## Reporting a bug

Report linter, formatter and language-server bugs in
[this repository](https://github.com/Qbox-project/qbx-lua/issues). Include the affected tool
and its version (`qbx-lint --version` or `qbx-lua-ls --version`), your platform, expected and actual
behavior, and a minimal Lua example. For CLI bugs, include the command you ran. For LSP bugs,
include the editor/LSP client, settings, server logs and the editor action that triggered the
problem. Include the manifest and relevant `qbxlint.toml` settings when the problem depends on
imports, side selection, or resource layout.

For a pull request, explain the resulting behavior and the checks you ran. Keep generated-data
updates identifiable in the diff. Release notes are generated from commit messages, so write them as [Conventional Commits](https://www.conventionalcommits.org).
