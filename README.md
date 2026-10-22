# jarsuite

**Install, update and launch a suite of Java tools that are released together.**

[jarexec](https://github.com/npedot/jarexec) ships one Java application as a small
launcher, a thin JAR and a lockfile. jarsuite builds on it for a *suite*: several
tools, each with its own repository and version, that share data through a common
library. Such tools are only safe together in matching versions, so the suite is
released as a whole (a release train). The client installs one release at a time,
atomically, and never runs two releases side by side.

```
dist/                                    published by `jarsuite assemble`
├── suite.toml                           manifest: release, tools, archive per platform
├── mysuite-2026.11.0-linux-x64.tar.gz   tool-a, tool-a.lock, tool-a-1.4.0.jar, tool-b…, lib/
└── mysuite-2026.11.0-windows-x64.zip
```

## Features

- **One release for the whole suite.** A hand-written `suite-def.toml` lists which tool
  versions form a release. `jarsuite assemble` turns it into one archive per platform.
- **Alignment checks.** Every tool must need the same Java, since the suite uses one JRE.
  Dependencies listed in `[align] required` must have the same version everywhere, or
  the release is refused. Other version differences are reported as warnings.
- **Shared cache.** All tools and releases use one jarexec cache: the JRE and the common
  dependencies are downloaded once.
- **Safe updates.** `update` downloads, verifies (SHA-256) and prepares a release while
  tools keep running. `activate` switches to it only when no tool is running.
- **Data-aware rollback.** A release declares its `data_format`. Going back to a release
  with a lower one needs `--force`, because the shared data may already be upgraded.

## At build time: compose a release

Each tool repository publishes what `jarexec lock` produces: `<tool>.lock`, the thin
JAR and `lib/` for bundled JARs. The suite repository holds only `suite-def.toml`:

```toml
[suite]
name = "mysuite"
release = "2026.11.0"
data_format = 3                       # optional: format of the data the tools share

[align]
required = ["com.example:commons"]    # same version in every tool, or no release

[jarexec]
version = "0.3.0"                     # jarexec release used as launcher
# platforms = ["linux-x64", "windows-x64", …]   default: all five

[[tool]]
name = "tool-a"
source = "https://example.com/tool-a/1.4.0/"   # or a folder; holds tool-a.lock
```

```bash
jarsuite assemble suite-def.toml -o dist --base-url https://example.com/mysuite/2026.11.0/
```

| Option | Purpose |
|---|---|
| `-o DIR` | output folder (default `dist`) |
| `--base-url URL` | where the archives will be published (default: next to `suite.toml`) |
| `--jarexec PLATFORM=PATH` | use a local jarexec binary instead of downloading the release |
| `--platform PLATFORM` | build only this platform (repeatable) |

Every JAR is checked against the SHA-256 in its lock, and the jarexec archive against
its `.sha256`.

## On the user's machine

```bash
jarsuite init https://example.com/mysuite/suite.toml   # once
jarsuite update                                        # install the published release
jarsuite run tool-a -- --some-option                   # launch a tool
```

| Command | Effect |
|---|---|
| `init <manifest>` | follow a published `suite.toml` (URL or path) |
| `status [--check]` | current, previous and pending release; `--check` also asks for the published one |
| `update [--no-activate]` | download, verify and prepare the published release, then activate it |
| `activate [RELEASE] [--force]` | switch to an installed release (default: the pending one) |
| `rollback [--force]` | go back to the previous release |
| `run <tool> [args…]` | launch a tool of the active release |
| `clean` | remove old releases and what the cache holds only for them |

The installation folder is `--root`, `$JARSUITE_ROOT`, or the folder of the jarsuite
binary. If `update` finds tools running, it leaves the release pending and exits with
code 2: close them and run `jarsuite activate`.

```
<root>/
├── jarsuite.toml   manifest URL and state
├── releases/<r>/   one folder per installed release (current, previous, pending)
└── .jarexec/       cache shared by every tool and release
```

### Window

`jarsuite-gui` does the same from a window: it shows the active, published, pending and
previous release, and has buttons for *Check*, *Update*, *Activate*, *Roll back* and
*Clean*, one *Launch* button per tool, and a progress bar for downloads. It is built
with the `gui` feature:

```bash
cargo build --release --features gui      # target/release/jarsuite-gui
jarsuite-gui [--root DIR]
```

It finds the installation folder like `jarsuite`, so it can sit next to it. On Windows
it opens no console; tools that need one open their own.

The *Arguments* field is passed to the tool you launch. The window starts tools in the
installation folder, whatever folder it was opened from, while `jarsuite run` keeps the
current one. When a tool exits with an error, the log shows the last lines it wrote to
standard error.

A tool launched with `jarsuite run` gets `JARSUITE_ROOT` and `JARSUITE_RELEASE` in its
environment. Only tools started this way count as running: a launcher started
directly from `releases/<r>/` is not seen by `activate`.

## Example

[`examples/`](examples) holds a small suite: a shared library and two tools that
read and write the same file. It shows a valid release, an upgrade of the data
format and a mixed release that `assemble` refuses.

## Current limitations

- jarsuite does not update itself.
- The window passes arguments to a tool split on spaces, with no quoting.
- The manifest is not signed: it relies on HTTPS, and the archives on their SHA-256.
- Private repositories with authentication are not supported, as in jarexec.

## Development

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --features gui -- -D warnings
(cd examples && ./build.sh && JARSUITE=path/to/jarsuite ./e2e.sh)   # end-to-end, needs Maven and JDK 21
```

CI runs these on Linux, Windows and macOS. A `v*` tag that matches the version in
`Cargo.toml` publishes `jarsuite` and `jarsuite-gui` for the five platforms as a
GitHub release.

## License

[MIT](LICENSE)
