# Example suite

Three small Maven modules that behave like a real suite of tools:

| Module | Role |
|---|---|
| `commons` | Shared library, not published anywhere. Reads and writes `data/shared.txt`, whose first line is `format=N`. It refuses files in a newer format and upgrades older ones on write. |
| `tool-a` | `tool-a add <text>`: appends an entry. Depends on `commons` and `commons-text` (which brings `commons-lang3` 3.14.0). |
| `tool-b` | `tool-b list`: prints the entries. Depends on `commons`, `commons-lang3` 3.17.0 and `slf4j-simple`. |

The data folder is `$JARSUITE_DATA`, or `data/` in the current directory.

`commons` 1.0.0 writes data format 1 and 2.0.0 writes format 2. The tools share the
file, so they are only safe together when they use the same `commons`. That is why the
suite is released as a whole instead of tool by tool.

## Build

Needs Maven, a JDK 21 and jarexec (`$JAREXEC`, the `PATH`, or built from `../../jarexec`):

```bash
./build.sh
```

It creates three folders in `out/`, each holding the renamed jarexec binaries, the
thin JARs, the locks and `lib/` with the bundled `commons`:

| Folder | Content | Expected |
|---|---|---|
| `aligned-1` | both tools on `commons` 1.0.0 | works |
| `aligned-2` | both tools on `commons` 2.0.0 | works, and upgrades format-1 data |
| `mixed` | `tool-a` on 2.0.0, `tool-b` on 1.0.0 | `tool-b list` fails once `tool-a` has written |

`commons-lang3` differs between the two tools in every folder. That misalignment is
harmless, while the one on `commons` is not: a suite launcher should warn about the
first and refuse the second.

## Try it

```bash
cd out/aligned-1 && ./tool-a add hello world && ./tool-b list
cd ../mixed      && ./tool-a add hello again && ./tool-b list   # fails: data format 2 is newer than supported 1
```

The tools in one folder share `.jarexec/`, so the second tool reuses the JRE and the
common dependencies downloaded by the first.

## As a jarsuite release

`suite-1.toml`, `suite-2.toml` and `suite-mixed.toml` turn the three folders into
suite releases 1.0, 2.0 and 3.0. With jarexec built locally (until its release is
published):

```bash
JX=path/to/jarexec; P=linux-x64
jarsuite assemble suite-1.toml -o pub/1 --platform $P --jarexec $P=$JX
jarsuite assemble suite-2.toml -o pub/2 --platform $P --jarexec $P=$JX
jarsuite assemble suite-mixed.toml -o pub/3 --platform $P --jarexec $P=$JX   # refused: commons not aligned

export JARSUITE_ROOT=$PWD/root JARSUITE_DATA=$PWD/data
jarsuite init pub/1/suite.toml && jarsuite update
jarsuite run tool-a add hello && jarsuite run tool-b list
jarsuite init pub/2/suite.toml && jarsuite update      # data format 2
jarsuite rollback                                      # refused: the data may be in format 2
```
