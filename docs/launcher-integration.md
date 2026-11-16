# Launcher integration

Legion is a command wrapper. Install the packaged executable somewhere stable,
then add its path once to the launcher's wrapper field. Keep the game executable,
installed Proton selection, Wine prefix and game arguments in their normal
launcher fields. No per-game `--proton` or `--prefix` command is required when
the launcher supplies that selection.

| Launcher | Setting | Value |
| --- | --- | --- |
| Steam | Game properties → Launch Options | `/absolute/path/to/legion %command%` |
| Faugus, native package | Game properties → Launch Arguments | `/absolute/path/to/legion` |
| Lutris, native package | Configure → System options → Command prefix | `/absolute/path/to/legion` |
| Heroic, native package | Game settings → Advanced → Wrapper | Executable `/absolute/path/to/legion`, no wrapper arguments |

Use an absolute path if Legion is not on the launcher's `PATH`. An environment
variable such as `LEGION=1` alone cannot start Legion: a launcher would need to
implement that hook. The wrapper fields already provide the required entry point.

For an environment toggle, set the wrapper command to `/absolute/path/to/legion
auto` once, using launcher-wide defaults where available. Then set the game's
environment variable `LEGION=1` to use the VM. With `LEGION` unset or `LEGION=0`,
the wrapper executes the original command unchanged, preserving its arguments,
environment, working directory, signals and exit status. Disabled mode does not
extract Legion's runtime or check KVM. Heroic separates the executable and
arguments: use Legion's path as the executable and `auto` as its arguments.
Faugus does not need `%command%`; use `legion auto` in its launch-prefix field.
Launchers without a global wrapper setting still need that prefix on each game.

Legion recognizes direct Proton commands, `umu-run game.exe` with `PROTONPATH`,
and Wine executable paths belonging to a Proton installation. It also reads
Lutris's `WINE` selection and resolves exact installed runner names in standard
Steam, UMU, Lutris and Heroic directories. It never chooses an arbitrary newer
Proton if the selected runner cannot be found. UMU's unresolved download tokens,
such as `GE-Proton` without a corresponding installed runner or selected Wine
path, require installing/selecting a concrete Proton runner in the launcher.

This path uses the installed Proton directly inside Legion's guest. It does
not execute UMU's host container or download/update a runner. Ordinary standalone
Wine builds, Linux-native games, UMU TOML launch files and setup utilities such
as winetricks are not supported by the game wrapper. Apply the wrapper to game
launches, not to launcher maintenance commands.

The launcher-selected prefix, game arguments and DLL overrides are retained.
An in-game-directory working directory is retained; otherwise the executable's
directory is used. Store identifiers, Proton/DXVK/VKD3D options and selected
Wine synchronization options are forwarded. Legion controls the guest's
Wayland display and Vulkan driver independently of the host launcher's display
settings. A MangoHud prefix is translated to the guest overlay setting. Place
host display wrappers such as gamescope before Legion if using them together.

For a read-only check, copy the launch command printed by the launcher and put
`legion inspect-launch --` before its runner and arguments, using the same
`PROTONPATH`, `WINEPREFIX` and other launcher environment settings. This prints
the resolved runner, executable, prefix/compatdata and argument boundaries. It
does not boot a VM, run the game or initialize the prefix. Shell quoting is the
caller's responsibility; Legion does not evaluate command strings.

If Vulkan setup fails, run `legion doctor --graphics` from a desktop terminal.
The run's `vm/report.json` records inherited host Vulkan driver overrides and
filters; `vm/qemu.log` contains host driver loading errors. Those overrides
must refer to host-visible 64-bit drivers.

Flatpak launchers require a separate host-execution integration: Legion needs
host KVM/graphics access and host-visible paths for the runner, prefix and game.
Native-package wrapper support does not establish Flatpak support. Simply
prepending `flatpak-spawn --host` also does not guarantee that the selected
runner environment and paths survive. No launcher source files or user game
entries are modified by installing Legion.

The launcher command shapes are covered by tests, including spaces, empty
arguments, literal shell syntax, named installed runners, and malformed nested
commands. This does not certify every store's authentication, DRM, overlay,
anti-cheat, Proton build or game compatibility.
