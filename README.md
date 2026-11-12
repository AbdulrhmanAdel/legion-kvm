# Legion KVM

Legion runs Windows applications through Proton inside a Linux KVM guest,
with a local desktop window and shared graphics access. The launcher detects
CPU topology and selects the graphics backend independently of CPU vendor.

The runtime includes NVIDIA and AMD graphics paths, guest initialization,
CPU compatibility profiles, display transport, input, audio and directory
sharing. AVX-512 is not a host requirement. The guest uses reduced XSTATE.
Hardware and graphics-driver support depend on the selected backend.

## Running

A packaged executable contains the guest kernel and runtime assets. Run it as
your desktop user with access to `/dev/kvm` and the graphics device. The desktop
presentation path requires Wayland.

Steam launch options:

```text
/path/to/legion %command%
```

Other launchers can place Legion before their Proton command. An explicit
launch accepts an installed Proton and an existing Wine prefix:

```sh
legion run --proton /path/to/Proton --prefix /path/to/prefix -- /path/to/application.exe
```

The selected prefix and home directory remain shared with the application.
Normal application and Proton writes reach those locations. Compatibility
repairs are applied inside the guest, and installed Proton files remain
unchanged. Selected Wine components must match the runtime compatibility
recipe; arbitrary Proton builds are not interchangeable with a given recipe.

Useful commands:

```sh
legion doctor
legion cpu-check
legion --version
```

`LEGION_RENDER_NODE` selects a DRM render node on systems with multiple GPUs.
`LEGION_MEMORY_MIB` selects guest memory; the default is 12288 MiB.
`LEGION_STARTUP_UI=auto|window|log|off` controls startup feedback. Runtime files
are cached under `XDG_CACHE_HOME/legion`; logs use `XDG_STATE_HOME/legion`.

## Building

The workspace requires Rust 1.99 and Linux development libraries for Wayland,
XKB, EGL, GBM, DRM, SDL2 and Vulkan. Build the Rust components with:

```sh
cargo build --release --workspace
```

The launcher binary is `target/release/legion`. A source build does not contain
a guest image. Packaging requires a prepared runtime manifest, guest kernel,
matching modules, guest userspace, QEMU, virtiofsd and the selected GPU backend.
Those components are built separately; they are not downloaded by the launcher.

The NVIDIA backend is a separate workspace with its original component
licenses:

```sh
cargo build --release --manifest-path backends/nvidia/Cargo.toml \
  -p device --features vhost-user --bin vhost-user-nvgpu
```

`kernel/build_guest.py` builds the guest kernel from a supplied Linux source
tree. `guest-cpuid/` contains the guest CPU-control module. Build all guest
modules against the exact guest kernel output tree, never against the host
kernel as an installation step.

`tools/` contains helpers for QEMU, RADV, userspace preparation, compatibility
recipes and runtime packaging. Runtime helper files live under
`/usr/share/legion` in the guest. To append a prepared runtime to a static
launcher:

```sh
python3 tools/build_single_file.py --runtime /path/to/runtime \
  --launcher /path/to/static/legion --output dist/legion
```

Host-side binaries and libraries must target a portable CPU baseline. Before
packaging, check the host tools without AVX-512:

```sh
python3 tools/verify_portable_host.py --runtime /path/to/runtime \
  --output /path/to/portability.json
```

## Development

```sh
cargo fmt --all --check
cargo check --workspace --all-targets --all-features
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

## License

The main Rust workspace is licensed under AGPL-3.0-only. NVIDIA backend
components retain their Apache-2.0, BSD-3-Clause and GPL licenses. Kernel and
QEMU components retain their respective upstream licenses. See `LICENSE` and
`NOTICE`.
