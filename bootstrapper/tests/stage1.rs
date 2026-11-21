//! End-to-end tests for stage 1 recipe loading and `.eva` package builds.

use std::path::{Path, PathBuf};

use evergreen_bootstrap::config::{Arch, BootstrapConfig};
use evergreen_bootstrap::stage1::{
    build_order, build_stage1, load_recipes, load_state, Stage1Paths,
};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bs-stage1-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn shipped_stage1_recipes_load_and_order() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../packages");
    let recipes = load_recipes(&dir, Arch::X86_64).unwrap();
    let names: Vec<&str> = recipes.iter().map(|recipe| recipe.name()).collect();
    for expected in [
        "pine",
        "eva",
        "pine-init",
        "evergreen-build",
        "evergreen-bootstrap",
        "evergreen-installer",
        "sudo-rs",
        "greetd",
        "linux-lts",
        "linux-stable",
        "btrfs-progs",
        "e2fsprogs",
        "cryptsetup",
        "dbus",
        "polkit",
        "elogind",
        "fish",
        "grub2",
        "shim",
        "mokutil",
        "linux-firmware",
        "evergreen-base",
        "evergreen-mirrors",
        "xorg-server",
        "xwayland",
        "mesa",
        "libinput",
        "gtk3",
        "xfce4-session",
        "xfwm4",
        "xfce4-panel",
        "xfdesktop",
        "thunar",
        "xfce4-settings",
        "agreety",
        "labwc",
        "wayland",
        "wayland-protocols",
        "wlroots",
        "pixman",
        "libdisplay-info",
        "libliftoff",
        "hwdata",
        "sway",
        "swaybg",
        "swayidle",
        "swaylock",
        "waybar",
        "hyprland",
        "hyprpaper",
        "hypridle",
        "hyprlock",
        "hyprwayland-scanner",
        "hyprutils",
        "hyprlang",
        "hyprgraphics",
        "hyprcursor",
        "mango",
        "mangobar",
        "niri",
        "nirinit",
        "nwg-displays",
        "kitty",
        "alacritty",
        "foot",
        "fcft",
        "tllist",
        "networkmanager",
        "nm-applet",
        "modemmanager",
        "pipewire",
        "wireplumber",
        "alsa-lib",
        "alsa-utils",
        "pavucontrol",
        "bluez",
        "blueman",
        "cups",
        "system-config-printer",
        "upower",
        "tlp",
        "thermald",
        "wpa_supplicant",
        "openssh",
        "helium",
        "mailcraft",
        "artcraft-office",
        "wordcraft",
        "gridcraft",
        "deckcraft",
        "pdfcraft",
        "photocraft",
        "vlc",
        "gparted",
        "flatpak",
        "xdg-desktop-portal",
        "xdg-desktop-portal-gtk",
        "xdg-desktop-portal-wlr",
        "ffmpeg",
        "x265",
        "libvpx",
        "dav1d",
        "opus",
        "flac",
        "libvorbis",
        "libogg",
        "artcraft-suite",
        "lightcraft",
        "vectorcraft",
        "designcraft",
        "figcraft",
        "texturecraft",
        "materialcraft",
        "filmcraft",
        "effectcraft",
        "anicraft",
        "soundcraft",
        "cadcraft",
        "craft-launcher",
        "papirus-icon-theme",
        "adwaita-icon-theme",
        "evergreen-gtk-theme",
        "evergreen-cursor-theme",
        "noto-fonts",
        "noto-cjk-fonts",
        "dejavu-fonts",
        "jetbrains-mono",
        "xfce4-terminal",
        "xfce4-whiskermenu-plugin",
        "xfce4-taskmanager",
        "xfce4-screenshooter",
        "xfce4-pulseaudio-plugin",
        "xfce4-battery-plugin",
        "xfce4-weather-plugin",
        "xfce4-clipman-plugin",
        "xfce4-notifyd",
        "xfce4-screensaver",
        "tuigreet",
        "gtk4",
        "fribidi",
        "graphene",
        "qt6-base",
        "extra-cmake-modules",
        "kcoreaddons",
        "plasma-workspace",
        "plasma-desktop",
        "kwin",
        "breeze",
        "sddm",
        "lightdm",
        "ly",
        "gdm",
        "dolphin",
        "konsole",
        "kate",
        "okular",
        "kdenlive",
        "krita",
        "kde-plasma",
        "kde-applications",
        "pine-service-units",
        "gnome",
        "gnome-shell",
        "mutter",
        "gnome-session",
        "gnome-control-center",
        "nautilus",
        "gnome-terminal",
    ] {
        assert!(
            names.contains(&expected),
            "missing stage 1 recipe: {expected}"
        );
    }

    for recipe in &recipes {
        if let Some(config) = &recipe.recipe.build.config_file {
            let path = recipe.directory.join(config);
            assert!(path.exists(), "missing config file {}", path.display());
        }
    }

    let order = build_order(&recipes).unwrap();
    let position = |name: &str| {
        order
            .iter()
            .position(|recipe| recipe.name() == name)
            .unwrap()
    };
    assert!(position("pine") < position("eva"));
    assert!(position("eva") < position("evergreen-build"));
    assert!(position("evergreen-build") < position("evergreen-bootstrap"));
}

#[test]
fn builds_stage1_packages_from_local_source_and_resumes() {
    let root = temp_dir("build");
    let recipes_dir = root.join("recipes");
    let source = recipes_dir.join("source");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("message.txt"), b"stage1 payload\n").unwrap();

    std::fs::write(
        recipes_dir.join("hello.toml"),
        r#"
[package]
name = "hello"
version = "1.0"
arch = "x86_64"
description = "Test stage 1 component"

[source]
path = "source"

[build]
script = "install -Dm644 message.txt \"$DESTDIR/usr/share/hello/message.txt\""

[stage1]
stage = 1
"#,
    )
    .unwrap();

    let config = BootstrapConfig {
        workdir: root.join("work"),
        ..BootstrapConfig::default()
    };
    let report = build_stage1(&config, &recipes_dir, false).unwrap();
    assert_eq!(report.built, vec!["hello"]);
    assert_eq!(report.packages.len(), 1);
    assert!(report.packages[0].exists());

    let paths = Stage1Paths::new(&config);
    assert!(paths.packages.join("hello-1.0.eva").exists());
    assert_eq!(
        load_state(&config)
            .unwrap()
            .get("hello")
            .unwrap()
            .to_string(),
        "Done"
    );

    let resumed = build_stage1(&config, &recipes_dir, true).unwrap();
    assert!(resumed.built.is_empty());
    assert_eq!(resumed.skipped, vec!["hello"]);
    assert_eq!(resumed.packages.len(), 1);

    std::fs::remove_dir_all(root).ok();
}
