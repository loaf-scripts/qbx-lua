# qbx-lua

**Qbox Lua (`qbx-lua`)** provides a **Lua linter, formatter, and language server for FiveM**. Both tools
share the same CfxLua parser, formatting engine, analysis rules and bundled FiveM data. They read
resource manifests to understand imports and client/server context.

| Tool | What it does | Guide |
| --- | --- | --- |
| `qbx-lint` | Lints and formats files from the command line or CI, with reports and automatic fixes. | [CLI usage](#use) |
| `qbx-lua-ls` | Provides editor diagnostics, completion, hover, navigation, rename, quick fixes and formatting over LSP. | [Language server and editors](#language-server-and-editors) |

The former `Qbox-project/qbx-lua-ls` repository was merged into
[`crates/qbx_lua_ls`](crates/qbx_lua_ls). From **v1.0.5**, releases here contain both executables;
the original commits, authors and historical tags are preserved. See the
[migration notes](docs/repository-migration.md) for what moved and where to contribute.

Editor adapters remain in [qbx-editor](https://github.com/Qbox-project/qbx-editor). Shared
analysis and language-server changes can now be developed and tested in one pull request here.

This repository was formerly named `qbx-lint`. Its new name describes the whole tooling suite;
the executable names remain `qbx-lint` and `qbx-lua-ls`. Existing GitHub links and Git remotes
redirect to `qbx-lua`; GitHub Action users must update their `uses:` address as shown below.

## Install

### Download binaries

Download the archive for your platform from the
[releases page](https://github.com/Qbox-project/qbx-lua/releases): choose `qbx-lint-<target>` for
the CLI or `qbx-lua-ls-<target>` for the language server. Each archive contains one executable
and its license. Extract the executable and put it on your `PATH`, or configure its absolute
path in your editor. `SHA256SUMS` contains checksums for both tools.

Both tools are available for Windows x64, Linux x64/ARM64 (musl), and macOS x64/ARM64.
The [Qbox Lua VS Code extension](https://marketplace.visualstudio.com/items?itemName=Qbox.qbx-lua)
bundles the server, so VS Code users can install the extension directly.

### Build from source

Use a stable Rust toolchain and Cargo. One checkout contains everything needed for both tools:

```sh
git clone https://github.com/Qbox-project/qbx-lua.git
cd qbx-lua

# Install either or both tools.
cargo install --path crates/qbx_lint --locked --profile lint-release
cargo install --path crates/qbx_lua_ls --locked

qbx-lint --version
qbx-lua-ls --version
```

Cargo installs the executable into its `bin` directory, which must be on your `PATH`.
For local builds without installation, run these commands separately from the workspace root:

```sh
cargo build --profile lint-release --locked -p qbx_lint
cargo build --release --locked -p qbx_lua_ls
```

The CLI is in `target/lint-release/qbx-lint` and the server is in `target/release/qbx-lua-ls`
(add `.exe` on Windows). The separate profiles preserve the CLI's abort-on-panic behavior and
the server's request recovery; see [CONTRIBUTING.md](CONTRIBUTING.md).

## Use

### Command-line linter and formatter

Run these commands from your resource or server checkout:

```sh
qbx-lint                                      # lint the current directory
qbx-lint 'resources/[qbx]'                     # lint a resource category
qbx-lint client/main.lua server/main.lua       # lint selected files
qbx-lint --fix                                # apply available fixes in place
qbx-lint --max-warnings 0                      # fail on warnings as well as errors
qbx-lint --format json --output lint.json      # write a report
qbx-lint --min-severity hint                   # include hint-level findings
qbx-lint --list-rules                          # show rules and their default levels
```

Output formats are `pretty`, `compact`, `json`, `junit`, `github`, and `sarif`. By default, the CLI
shows findings at `info` level and above. Lint exits with `1` for errors or an exceeded warning
limit, and `2` for an invocation or processing failure. `--no-fail` suppresses failures caused
by lint findings.

To format files:

```sh
qbx-lint fmt
qbx-lint fmt --check
qbx-lint fmt --config qbxlint.toml client/main.lua
```

`fmt --check` leaves files untouched and exits with `1` if formatting is needed. Formatting also
fails if a source file cannot be formatted. The formatter verifies tokens and comments before
writing. Both formatting and automatic fixes reject non-UTF-8 source; escrow, binary and
obfuscated files are skipped.

## Language server and editors

`qbx-lua-ls` provides:

- Diagnostics and quick fixes using the same rules as the CLI, with resource and client/server context.
- Completion, hover and signature help for Lua symbols, FiveM natives, exports, events and LuaCATS annotations.
- Definitions, references, rename, semantic tokens and inlay hints.
- Whole-document formatting using the shared `qbxlint.toml` settings.

For VS Code, install [Qbox Lua](https://marketplace.visualstudio.com/items?itemName=Qbox.qbx-lua)
and open a resource folder or your server's `resources` folder. The extension includes the server.
For Zed, Neovim, Helix and other LSP clients, see the
[editor setup guide](https://github.com/Qbox-project/qbx-editor/blob/main/docs/editors.md).

The server communicates over standard input and output using the Language Server Protocol.
An editor starts `qbx-lua-ls` without arguments; `--version` checks the installed version.
It does not need a running FiveM server. See the [server guide](crates/qbx_lua_ls/README.md)
for the full feature list and limits, and the
[LSP configuration and protocol reference](crates/qbx_lua_ls/docs/protocol.md) for client settings.

## Configure

Both the CLI and language server use `qbxlint.toml` for lint rules and formatting.
The language server also accepts editor settings for library paths, diagnostics, inlay hints
and semantic tokens; see its [configuration guide](crates/qbx_lua_ls/README.md#configuration).

Each Lua file follows every `qbxlint.toml` or `.qbxlint.toml` in its folder and the folders above
it, nearer files taking precedence, up to one that says `root = true`. Use
`--config path/to/qbxlint.toml` to apply one file to everything instead. Paths in configuration
patterns are relative to the directory of the file they are written in.

```toml
exclude = ["web/**", "**/vendor/**"]
ignore_diagnostics = ['\[standalone\]/']
globals = ["SomeRuntimeGlobal"]

[rules]
"unused-argument" = "off"
"fivem/citizen-prefix" = "warning"

[[overrides]]
files = ["tests/**"]
rules = { "undefined-global" = "off" }

[format]
indent_width = 4
use_tabs = false
line_width = 120
quote_style = "preserve"
```

See the [configuration and analysis reference](docs/reference.md), the
[rule list](docs/rules.md), and the [example configuration](examples/qbxlint.toml).

## GitHub Actions

Use the suite's lint action in your workflow:

```yaml
- uses: actions/checkout@v4
- uses: Qbox-project/qbx-lua@v1.0.6
  with:
    version: v1.0.6
    paths: .
    args: --max-warnings 0
```

The action emits GitHub annotations and writes a SARIF report. See
[examples/lint.yml](examples/lint.yml) and [action.yml](action.yml) for its inputs and output.

If you previously used `Qbox-project/qbx-lint@...`, change it to
`Qbox-project/qbx-lua@v1.0.6`. GitHub does not redirect Action references after repository renames;
see the [migration notes](docs/repository-migration.md#repository-rename-and-github-action).

## Limits

Analysis depends on the files available in the run. Dynamic names, runtime loading, encrypted
scripts, and JavaScript or C# resources limit what it can infer. Native definitions are bundled
data, not a live lookup. Security rules flag patterns for review; a clean report does not establish
that a resource is secure. Review automatic edits and test resource behavior in FiveM.

## Contribute

Report linter, formatter, shared analysis and language-server bugs in
[this repository's issues](https://github.com/Qbox-project/qbx-lua/issues), and submit changes
here. Editor adapter and UI issues belong in
[qbx-editor](https://github.com/Qbox-project/qbx-editor/issues).

See [CONTRIBUTING.md](CONTRIBUTING.md) for the crate layout, checks, and data generation commands.
Release notes are on the [releases page](https://github.com/Qbox-project/qbx-lua/releases).

## License

[GPL-3.0-or-later](LICENSE).
