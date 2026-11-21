# Evergreen Linux

A daily-driveable, user-friendly Linux distribution whose core system is written in Rust.

- **Init system:** `pine`
- **Package manager:** `eva`
- **Bootstrapper:** `evergreen-bootstrap`
- **Desktop:** XFCE4
- **Versioning:** CalVer (`YYYY.MM`)

## Repository layout

```
evergreen/
├── bootstrapper/       # Evergreen Bootstrapper (Rust) + seed + stage recipes
│   ├── src/
│   ├── seed/           # minimal binary seed (static busybox + skeleton)
│   └── recipes/
│       └── stage0/     # mini-bootstrap (~25 essential packages)
├── toolchain/          # Rust workspace crates
│   ├── common/         # shared recipes, version, repo types
│   ├── pine/           # init system (PID 1) + pinectl
│   ├── eva/            # package manager
│   ├── installer/      # evergreen-installer (GUI + TUI)
│   ├── build/          # evergreen-build orchestrator
│   └── pine-init/      # static initramfs init
├── packages/           # eva package recipes (Stage 1 builds from here)
│   ├── core/           # Evergreen components, bootstrapping utilities
│   ├── base/           # filesystem, encryption, D-Bus, bootloader, kernels
│   ├── desktop/        # X11, XFCE, Wayland
│   ├── nonfree/        # firmware
│   ├── extra/  community/  testing/
├── kernel/             # Evergreen kernel defconfig + patches
├── initramfs/          # initramfs profile + etc
├── iso/                # live ISO profiles (desktop + minimal)
├── branding/           # icons, themes, wallpapers
├── docs/               # design, guides, reference
├── scripts/            # developer/release helper scripts
└── ci/                 # CI configuration
```

## Design documents

The full plan lives outside this tree for now, under
`/home/deadbxnd/.opencode/plan/`:

- `evergreen-linux.md` — master plan
- `pine-internals.md` — init system internals
- `eva-resolver.md` — package resolver design
- `evergreen-bootstrapper.md` — bootstrapper design

## Building

The workspace uses the stable Rust toolchain (see `rust-toolchain.toml`).

```sh
cargo check --workspace --all-targets
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Binaries:

```sh
cargo run -p pine -- --version        # init system
cargo run -p eva -- --version         # package manager
cargo run -p evergreen-bootstrap -- --version
cargo run -p evergreen-build -- --version
cargo run -p evergreen-installer -- --version
cargo run -p pine-init -- --version
```

## Trying `pine` in a sandbox

`pine` can supervise services against an alternate root without being PID 1:

```sh
# 1. Create a unit tree.
mkdir -p /tmp/pine-demo/etc/pine/services
cat >/tmp/pine-demo/etc/pine/services/demo.service <<'EOF'
[Unit]
Description = "Demo service"

[Service]
Type = "simple"
ExecStart = "/bin/sh -c 'echo hello; sleep 300'"
Restart = "on-failure"
EOF
cat >/tmp/pine-demo/etc/pine/services/demo.target <<'EOF'
[Unit]
Wants = ["demo.service"]
EOF

# 2. Run the manager and talk to it.
cargo run -p pine -- --root /tmp/pine-demo --test --target demo.target &
cargo run -p pine --bin pinectl -- --socket /tmp/pine-demo/run/pine/control.sock list
cargo run -p pine --bin pinectl -- --socket /tmp/pine-demo/run/pine/control.sock status demo.service
cargo run -p pine --bin pinectl -- --socket /tmp/pine-demo/run/pine/control.sock log demo.service
cargo run -p pine --bin pinectl -- --socket /tmp/pine-demo/run/pine/control.sock poweroff
```

`pine --dump-units --root <root>` parses every unit and reports warnings without
starting anything.

## Status

`pine` is implemented: TOML units with drop-ins, dependency transactions and
target ordering, service supervision (simple/oneshot/forking), restart policies
and start limits, conditions, conflicts, `OnFailure`/`OnSuccess` hooks, cgroups
v2 limits, mount/swap units, per-unit logs, signals, shutdown, and a JSON control
socket served by `pinectl`. Unsupported features (D-Bus, notify/watchdog,
socket activation, templates, `Type=dbus`) are documented in the design and
degrade gracefully.

`eva` is implemented: signed repository indexes (ed25519, file/HTTP(S) sources),
TOML package metadata, a backtracking solver with provides/conflicts/replaces,
optional dependencies and recommends, pins/holds, ordered transaction plans,
SQLite installed-file database, staged `.eva` (tar.zst) installation with
journaling and rollback (including symlinks and usr-merge), file/DB
verification, locking, repository and key management, source builds, and the
full `eva` command set. Source builds use a staged directory with resolved
build dependencies; entering a clean chroot is delegated to `evergreen-build`.

The bootstrapper's stage 0 executor is implemented: `bootstrapper.toml` parsing,
pinned multi-pass stage 0 recipes, source fetch/checksum/extract/build/install,
per-package state with `--resume`, staging, and zstd tarball output. The first
recipes are in `bootstrapper/recipes/stage0/` (35 recipes covering both glibc
and musl editions, x86_64/aarch64 Rust seeds). Batch 0 validator packages live
in `packages/core/`: `evergreen-filesystem`, `evergreen-keyring`, `pine-units`,
and `uutils-coreutils`.

Stage 1 is implemented: `evergreen-bootstrap stage1` builds Rust components as
`.eva` packages into `<workdir>/packages/<arch>/`, with dependency ordering,
per-package state, and `--resume`. Stage 1 recipes live in the repository's
`packages/` tree (`core/`, `base/`, `desktop/`, `nonfree/`), split by purpose.
`packages/core/` covers `pine`, `pinectl` (in the `pine` package), `eva`,
`pine-init`, `evergreen-build`, `evergreen-bootstrap`, `evergreen-installer`,
`sudo-rs`, and `greetd`.

XFCE goodies and session integration are in Stage 1: `xfce4-terminal`,
`xfce4-whiskermenu-plugin`, `xfce4-taskmanager`, `xfce4-screenshooter`,
`xfce4-pulseaudio-plugin`, `xfce4-battery-plugin`, `xfce4-weather-plugin`,
`xfce4-clipman-plugin`, `xfce4-notifyd`, `xfce4-screensaver`, and the `tuigreet`
greetd greeter.

GNOME support is in Stage 1: `mutter`, `gnome-shell`, `gnome-session`,
`gnome-settings-daemon`, `gnome-control-center`, `nautilus`, `gnome-terminal`,
`gnome-keyring`, GNOME apps, and the `gnome` meta package.

All daemon packages are wired into pine via `pine-service-units`, which ships
TOML units and `.wants` links for D-Bus, polkit, elogind, NetworkManager,
wpa_supplicant, PipeWire, WirePlumber, BlueZ, CUPS, upower, chrony, thermald,
tlp, nftables, sshd and every display manager. Rootfs assembly also generates
these units as a fallback.

Display managers in Stage 1: `sddm` (KDE default), `lightdm`, `ly`, and `gdm`,
alongside the existing `greetd` and `agreety`/`tuigreet` options.

KDE support is in Stage 1: Qt 6 base stack, core KDE Frameworks 6, Plasma
(`plasma-workspace`, `plasma-desktop`, `kwin`, `breeze`, `sddm`), KDE applications
(`dolphin`, `konsole`, `kate`, `okular`, `kdenlive`, `krita`, `ark`, `gwenview`,
`kcalc`), and `kde-plasma` / `kde-applications` meta packages. All use dynamic
version resolution against the official GitHub mirrors.

`evergreen-installer` is fully implemented as a library and CLI: configuration
validation, automatic btrfs/LUKS partitioning, rootfs installation, fstab/locale/
timezone/hostname/pine configuration, user creation, GRUB installation, initial
snapshot, a text-mode wizard, and an optional GTK4 front end (`--gui`, built
with `--features gtk`). It is packaged in `packages/core/evergreen-installer.toml`.

Theming and fonts are in Stage 1: `papirus-icon-theme`,
`adwaita-icon-theme`, `evergreen-gtk-theme` and `evergreen-cursor-theme` (with
assets under `branding/`), `noto-fonts`, `noto-cjk-fonts`, `dejavu-fonts`, and
`jetbrains-mono`.

Default desktop applications are in Stage 1: `helium` (official prebuilt Linux
binary), `mailcraft`, the `artcraft-office` meta-package (`wordcraft`,
`gridcraft`, `deckcraft`, `pdfcraft`), `photocraft`, `vlc`, and `gparted`, plus
the full `artcraft-suite` (`lightcraft`, `vectorcraft`, `designcraft`,
`figcraft`, `texturecraft`, `materialcraft`, `filmcraft`, `effectcraft`,
`anicraft`, `soundcraft`, `cadcraft`, `craft-launcher`), plus
`flatpak` and the `xdg-desktop-portal` GTK/wlroots backends. Codec packages
(`ffmpeg`, `x265`, `libvpx`, `dav1d`, `opus`, `flac`, `libvorbis`, `libogg`) live
in `nonfree`.

The system services batch is in Stage 1: `wpa_supplicant`, `networkmanager`,
`nm-applet`, `modemmanager`, `pipewire`, `wireplumber`, `alsa-lib`,
`alsa-utils`, `pavucontrol`, `bluez`, `blueman`, `cups`,
`system-config-printer`, `upower`, `tlp`, `thermald`, and `openssh`.

The X11 + XFCE core batch is in Stage 1: X11 protocol/library stack
(`xorgproto` through `libxfont2`), Mesa/libdrm/libinput/Xorg/Xwayland, the
GTK3 stack (`glib`, `cairo`, `pango`, `gdk-pixbuf`, `gtk3`, fonts), and XFCE 4.20
(`libxfce4util`, `xfconf`, `garcon`, `exo`, `libxfce4ui`, `xfwm4`,
`xfce4-panel`, `xfdesktop`, `thunar`, `xfce4-settings`, `xfce4-session`,
`tumbler`) plus the `agreety` greeter. All use pinned upstream sources and
checksums. Wayland support is in `packages/desktop/`: `wayland`,
`wayland-protocols`, `pixman`, `hwdata`, `libdisplay-info`, `libliftoff`,
`wlroots`, the `labwc` beta compositor, the `sway` tiling WM with `swaybg`,
`swayidle`, `swaylock`, and `waybar`, plus Hyprland (`hyprland`, `hyprpaper`,
`hypridle`, `hyprlock`, and its `hyprutils`/`hyprlang`/
`hyprgraphics`/`hyprcursor`/`hyprwayland-scanner` libraries, `mango` with
`mangobar`, and `niri` with `nirinit` and `nwg-displays`. Terminal emulators
include `kitty`, `alacritty`, and `foot` (with `fcft`/`tllist`). Mesa is built
with X11 and Wayland platforms enabled.

Image assembly and ISO builds are implemented. `evergreen-build assemble`
extracts the stage 0 tarball, installs all `.eva` packages from the given
directories into a rootfs, registers them in eva's database, and writes base
configuration and pine units. `evergreen-build iso` packages a rootfs into a
live squashfs and a GRUB EFI/BIOS ISO using `mksquashfs` and `grub-mkrescue`.
The equivalent `evergreen-bootstrap assemble` / `iso` commands use defaults
derived from the bootstrap workdir.

Repository publishing is implemented: `eva index <dir> --sign <key>` generates
a signed `index.toml` from `.eva` archives, `eva key generate` / `fingerprint`
manage ed25519 keys, and `evergreen-build publish` copies packages into a repo
directory and writes the signed index. The production public key is packaged by
`evergreen-keyring` from `ci/keys/evergreen.pub`. CI workflows under
`.github/workflows/` run Rust checks, recipe validation, an index-signing smoke
test, a scheduled dynamic-resolution matrix, scheduled **kernel package builds**
(`kernel.yml`, with Rust kernel config disabled via `EVERGREEN_KERNEL_NO_RUST=1`
on CI), and a scheduled **nightly minimal ISO build** (`nightly-iso.yml`) that
downloads the LTS kernel artifact and assembles a bootable ISO.

Package recipes resolve upstream versions dynamically. URLs use a `{version}`
(or `{tag}`) placeholder and an empty `sha256`; `eva build` queries the
upstream API/listing, picks the newest stable version, downloads the source,
computes and caches its checksum in `<root>/var/cache/eva/resolved/`, and
exports the chosen version as `$VERSION` to the build script. GitHub, GitLab,
crates.io, and directory listings are supported. Use `eva build --refresh` to
force an update check or `eva source resolve <recipe>` to inspect the result.
Maintainers only need to touch a recipe when its build steps need attention.

The Boot & base package batch is in Stage 1: filesystem tools (`btrfs-progs`,
`e2fsprogs`, `dosfstools`, `xfsprogs`, `fuse`), encryption (`cryptsetup`), the D-Bus
reference daemon with `polkit` and `elogind`, base utilities (`fish`, `tzdata`,
`nftables`, `chrony`), bootloader tooling (`efivar`, `efibootmgr`, `gnu-efi`,
`grub2`, `shim`, `mokutil`), `linux-firmware`, and the `evergreen-base` /
`evergreen-mirrors` meta packages. All use pinned upstream sources and checksums.

Two kernel packages are in Stage 1: `linux-lts` (6.18.64 longterm, the default)
and `linux-stable` (7.3.5 stable). Both use the shared defconfig in
`kernel/config/evergreen.defconfig` and install `/boot/vmlinuz-<version>`,
modules, and boot config. They provide the virtual `kernel` name and conflict
with each other. Assembly generates `/boot/initramfs` from the installed
`pine-init` binary and the newest kernel's modules, and the ISO builder selects
`boot/vmlinuz` or the newest versioned image automatically. The installer
remains to be implemented.
