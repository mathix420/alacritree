# Vendored ConPTY host

Windows hosts every PTY in a console server process. The one in the box
(`conhost.exe`) is the throughput bottleneck for terminal output: on a
Ryzen 9 5900X it turns termbench's `TermMarkV2 Small` into a 38–41 s run,
against 5.4 s for the build vendored here.

`alacritty_terminal` already prefers a side-by-side host when one is present —
`tty/windows/conpty.rs` calls `LoadLibraryW("conpty.dll")` and falls back to
`kernel32`'s `CreatePseudoConsole` when that fails. Both files below have to
sit in the same directory as `alacritree.exe`: `harden_dll_search_path` drops
PATH and the working directory from the DLL search order, deliberately leaving
the executable's own directory as the only non-system place a `conpty.dll` can
come from. That is what stops another terminal's install directory on PATH from
hosting our panes.

`conpty.dll` alone does nothing useful — it is a shim that launches
`OpenConsole.exe` as the console server. Both must ship together.

Absence is not an error. A build without these files logs
`Using Windows API for pseudoconsole` and runs on the inbox host, just slower.

## Contents

| file | source path inside the package |
| --- | --- |
| `conpty.dll` | `runtimes/win-x64/native/conpty.dll` |
| `OpenConsole.exe` | `build/native/runtimes/x64/OpenConsole.exe` |
| `LICENSE-conpty.txt` | `microsoft/terminal` repository root |

x64 only, matching the single Windows target in `dist-workspace.toml`. The
package also carries arm64 and x86 builds if that target list ever grows.

## Provenance

- Package: `Microsoft.Windows.Console.ConPTY` 1.25.260710002-preview
- File version: 1.25.2607.10002
- Release: `v1.25.1912.0` of <https://github.com/microsoft/terminal>
- License: MIT (`© Microsoft Corporation`), per the package's `.nuspec`

```
sha256  e2fe87e2258c4e46ffc5157f727218cc25f34a174902f72eb8a5b49edd9a6458  conpty.dll
sha256  2525c351aa136d555e5df9a3c9d6ce9be43f785e37e3c993b8f23b3f0a53c7fa  OpenConsole.exe
```

## Updating

```sh
gh release download <tag> -R microsoft/terminal -p '*ConPTY*.nupkg'
unzip -o Microsoft.Windows.Console.ConPTY.*.nupkg -d conpty_pkg
cp conpty_pkg/runtimes/win-x64/native/conpty.dll        alacritree/vendor/conpty/
cp conpty_pkg/build/native/runtimes/x64/OpenConsole.exe alacritree/vendor/conpty/
```

Then refresh the version, release tag, and hashes above. Benchmark before and
after: the reason to carry a megabyte of someone else's binary is throughput,
so a bump that does not move termbench is a bump worth skipping.
