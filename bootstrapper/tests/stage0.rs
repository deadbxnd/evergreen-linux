//! End-to-end tests for stage 0 recipe loading and build execution.

use std::path::{Path, PathBuf};
use std::process::Command;

use evergreen_bootstrap::build::{build_stage0, load_state, preflight, Stage0Paths};
use evergreen_bootstrap::config::{Arch, BootstrapConfig, Libc};
use evergreen_bootstrap::recipe::{build_order, load_recipes};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bs-it-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn sha256(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).unwrap();
    hex::encode(Sha256::digest(bytes))
}

#[test]
fn shipped_stage0_recipes_load_and_order() {
    let recipes = Path::new(env!("CARGO_MANIFEST_DIR")).join("recipes/stage0");
    let loaded = load_recipes(&recipes, Libc::Glibc, Arch::X86_64).unwrap();
    let names: Vec<&str> = loaded.iter().map(|recipe| recipe.name()).collect();
    assert!(names.contains(&"linux-headers"));
    assert!(names.contains(&"libc"));
    assert!(names.contains(&"rust"));
    assert!(!names.contains(&"busybox"), "musl recipe leaked into glibc");

    let order = build_order(&loaded).unwrap();
    let position = |name: &str| {
        order
            .iter()
            .position(|recipe| recipe.name() == name)
            .unwrap()
    };
    assert!(position("linux-headers") < position("binutils-pass1"));
    assert!(position("binutils-pass1") < position("gcc-pass1"));
    assert!(position("gcc-pass1") < position("libc"));
    assert!(position("libc") < position("gcc-pass2"));
    assert!(position("gcc-pass2") < position("binutils-pass2"));
    assert!(position("binutils-pass2") < position("make"));
    assert!(position("curl") < position("rust"));

    let musl = load_recipes(&recipes, Libc::Musl, Arch::X86_64).unwrap();
    let musl_coreutils = musl
        .iter()
        .find(|recipe| recipe.name() == "coreutils")
        .unwrap();
    assert_eq!(musl_coreutils.version(), "1.36.1");
    assert!(
        !loaded.iter().any(|recipe| recipe.version() == "1.36.1"),
        "busybox recipe leaked into glibc"
    );

    let aarch64 = load_recipes(&recipes, Libc::Glibc, Arch::Aarch64).unwrap();
    assert_eq!(
        aarch64
            .iter()
            .find(|recipe| recipe.name() == "rust")
            .unwrap()
            .version(),
        "1.85.0"
    );
}

#[test]
fn builds_stage0_from_local_recipes_and_resumes() {
    let root = temp_dir("build");
    let recipes_dir = root.join("recipes");
    let source_dir = root.join("source");
    std::fs::create_dir_all(&recipes_dir).unwrap();
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("hello.txt"), b"stage0 payload\n").unwrap();

    let archive = root.join("hello-1.0.tar.gz");
    let status = Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(&source_dir)
        .arg("hello.txt")
        .status()
        .unwrap();
    assert!(status.success());
    let checksum = sha256(&archive);

    std::fs::write(
        recipes_dir.join("hello.toml"),
        format!(
            r#"
[package]
name = "hello"
version = "1.0"
arch = "any"

[source]
path = "{archive}"
sha256 = "{checksum}"
strip_components = 0

[bootstrap]
stage = 0
pass = 0
editions = ["both"]
configure = ""
build = ""
install = "install -Dm644 hello.txt '{{sysroot}}/usr/share/hello/message.txt'"
"#,
            archive = archive.display()
        ),
    )
    .unwrap();

    let config = BootstrapConfig {
        workdir: root.join("work"),
        artifacts: PathBuf::from("artifacts"),
        ..BootstrapConfig::default()
    };
    let report = build_stage0(&config, &recipes_dir, false).unwrap();
    assert_eq!(report.built, vec!["hello"]);
    let paths = Stage0Paths::new(&config);
    assert_eq!(
        std::fs::read_to_string(paths.sysroot.join("usr/share/hello/message.txt")).unwrap(),
        "stage0 payload\n"
    );
    assert!(report.tarball.exists());
    assert_eq!(
        load_state(&config)
            .unwrap()
            .get("hello")
            .unwrap()
            .to_string(),
        "Done"
    );

    let resumed = build_stage0(&config, &recipes_dir, true).unwrap();
    assert!(resumed.built.is_empty());
    assert_eq!(resumed.skipped, vec!["hello"]);

    let checks = preflight();
    assert!(checks.iter().any(|check| check.name == "tar" && check.ok));

    std::fs::remove_dir_all(root).ok();
}

#[test]
fn rejects_wrong_source_checksum() {
    let root = temp_dir("checksum");
    let recipes_dir = root.join("recipes");
    let source_dir = root.join("source");
    std::fs::create_dir_all(&recipes_dir).unwrap();
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("hello.txt"), b"payload\n").unwrap();
    let archive = root.join("hello-1.0.tar.gz");
    assert!(Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(&source_dir)
        .arg("hello.txt")
        .status()
        .unwrap()
        .success());

    std::fs::write(
        recipes_dir.join("hello.toml"),
        format!(
            r#"
[package]
name = "hello"
version = "1.0"

[source]
path = "{}"
sha256 = "{}"

[bootstrap]
stage = 0
pass = 0
editions = ["both"]
configure = ""
build = ""
install = ""
"#,
            archive.display(),
            "0".repeat(64)
        ),
    )
    .unwrap();

    let config = BootstrapConfig {
        workdir: root.join("work"),
        ..BootstrapConfig::default()
    };
    assert!(build_stage0(&config, &recipes_dir, false).is_err());
    std::fs::remove_dir_all(root).ok();
}
