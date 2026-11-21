//! End-to-end tests: a signed local repository, then install/upgrade/remove,
//! rollback, verification, source builds, and repository/key management.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use eva::archive::{sha256_file, write_package};
use eva::model::{Arch, Candidate, PackageId, PackageMeta};
use evergreen_common::{RepoId, Version};

fn temp_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "eva-e2e-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(root.join("etc/eva/keys")).unwrap();
    std::fs::create_dir_all(root.join("var/cache/eva")).unwrap();
    root
}

fn candidate(name: &str, version: &str, _file: &str) -> Candidate {
    Candidate {
        id: PackageId {
            name: name.to_string(),
            version: Version::parse(version).unwrap(),
            arch: Arch::NoArch,
            repo: RepoId(0),
        },
        meta: PackageMeta {
            description: format!("{name} {version}"),
            size_installed: 16,
            ..PackageMeta::default()
        },
        deps: Vec::new(),
        opt_deps: Vec::new(),
        recommends: Vec::new(),
        suggests: Vec::new(),
        conflicts: Vec::new(),
        provides: Vec::new(),
        replaces: Vec::new(),
        files: Vec::new(),
        symlinks: Vec::new(),
        hooks: Default::default(),
        archive: String::new(),
    }
}

fn create_package(repo: &Path, candidate: &Candidate, content: &str) -> String {
    let payload = repo.join(format!("payload-{}", candidate.id.name));
    let relative = format!("usr/share/{}/message.txt", candidate.id.name);
    std::fs::create_dir_all(payload.join(format!("usr/share/{}", candidate.id.name))).unwrap();
    std::fs::write(payload.join(&relative), content).unwrap();
    if candidate.id.name == "lib" {
        std::os::unix::fs::symlink("usr/bin", payload.join("bin")).unwrap();
    }

    let archive = repo.join(candidate.archive_name());
    write_package(&archive, candidate, &payload).unwrap();
    let checksum = sha256_file(&archive).unwrap();
    std::fs::remove_dir_all(payload).ok();
    checksum
}

fn index_entry(candidate: &Candidate, checksum: &str, dependencies: &str) -> String {
    format!(
        "[[packages]]\nname = \"{}\"\nversion = \"{}\"\narch = \"{}\"\ndescription = \"{}\"\nchecksum = \"{}\"\nsize_installed = 16\nsize_download = 64\n{dependencies}\n",
        candidate.id.name,
        candidate.id.version,
        candidate.id.arch,
        candidate.meta.description,
        checksum
    )
}

fn setup_repo(root: &Path, version: &str) -> PathBuf {
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();

    let lib = candidate("lib", "1.0", "");
    let app = candidate("app", version, "");
    let lib_checksum = create_package(&repo, &lib, "lib payload\n");
    let app_checksum = create_package(&repo, &app, "app payload\n");

    let mut index = format!(
        "[metadata]\nname = \"core\"\nbranch = \"stable\"\n\n{}\n{}\n",
        index_entry(&lib, &lib_checksum, ""),
        index_entry(
            &app,
            &app_checksum,
            "[packages.dependencies]\nlib = \">=1.0\"\n"
        )
    );
    if version == "2.0" {
        index.push_str(&index_entry(
            &candidate("app", "2.0", ""),
            &app_checksum,
            "[packages.dependencies]\nlib = \">=1.0\"\n",
        ));
    }

    use ed25519_dalek::{Signer, SigningKey};
    let key = SigningKey::from_bytes(&[9u8; 32]);
    std::fs::write(
        root.join("etc/eva/keys/test.pub"),
        format!("{}\n", hex::encode(key.verifying_key().to_bytes())),
    )
    .unwrap();
    let signature = key.sign(index.as_bytes());
    std::fs::write(repo.join("index.toml"), &index).unwrap();
    std::fs::write(
        repo.join("index.toml.sig"),
        hex::encode(signature.to_bytes()),
    )
    .unwrap();

    std::fs::write(
        root.join("etc/eva/repos.toml"),
        format!(
            "[[repo]]\nname = \"core\"\nurl = \"file://{}\"\nbranch = \"stable\"\npriority = 0\nenabled = true\n",
            repo.display()
        ),
    )
    .unwrap();
    repo
}

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_eva"))
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .expect("run eva")
}

fn assert_ok(output: &Output, label: &str) {
    assert!(
        output.status.success(),
        "{label} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn installs_upgrades_removes_and_rolls_back() {
    let root = temp_root("flow");
    setup_repo(&root, "1.0");

    assert_ok(&run(&root, &["sync"]), "sync");

    let list = run(&root, &["list", "--available"]);
    assert_ok(&list, "list available");
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(stdout.contains("app"), "{stdout}");
    assert!(stdout.contains("lib"), "{stdout}");

    assert_ok(&run(&root, &["install", "app", "-y"]), "install");
    assert_eq!(
        std::fs::read_to_string(root.join("usr/share/app/message.txt")).unwrap(),
        "app payload\n"
    );
    assert!(root.join("usr/share/lib/message.txt").exists());
    assert_eq!(
        std::fs::read_link(root.join("bin")).unwrap(),
        std::path::PathBuf::from("usr/bin")
    );

    let verify = run(&root, &["verify"]);
    assert_ok(&verify, "verify");
    assert!(String::from_utf8_lossy(&verify.stdout).contains("verified"));

    // Publish a newer version and sync again.
    setup_repo(&root, "2.0");
    assert_ok(&run(&root, &["sync"]), "sync upgrade");
    assert_ok(&run(&root, &["upgrade", "app", "-y"]), "upgrade");
    assert_eq!(
        std::fs::read_to_string(root.join("usr/share/app/message.txt")).unwrap(),
        "app payload\n"
    );
    let info = run(&root, &["info", "app", "--json"]);
    assert_ok(&info, "info");
    let value: serde_json::Value = serde_json::from_slice(&info.stdout).unwrap();
    assert_eq!(value["installed"], "2.0");

    // Roll back the upgrade, then remove the package tree.
    assert_ok(&run(&root, &["rollback"]), "rollback");
    let info = run(&root, &["info", "app", "--json"]);
    let value: serde_json::Value = serde_json::from_slice(&info.stdout).unwrap();
    assert_eq!(value["installed"], "1.0");

    assert_ok(&run(&root, &["remove", "app", "-y"]), "remove");
    assert!(!root.join("usr/share/app/message.txt").exists());
    assert!(!root.join("usr/share/lib/message.txt").exists());
    assert!(std::fs::symlink_metadata(root.join("bin")).is_err());

    let journals: Vec<_> = std::fs::read_dir(root.join("var/lib/eva/journal"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert!(journals.len() >= 2);

    std::fs::remove_dir_all(root).ok();
}

#[test]
fn detects_tampered_index_and_rejects_unsigned() {
    let root = temp_root("signature");
    let repo = setup_repo(&root, "1.0");

    let mut index = std::fs::read_to_string(repo.join("index.toml")).unwrap();
    index.push_str("\n# tampered\n");
    std::fs::write(repo.join("index.toml"), index).unwrap();

    let sync = run(&root, &["sync"]);
    assert!(!sync.status.success());
    assert!(String::from_utf8_lossy(&sync.stderr).contains("signature"));

    assert_ok(&run(&root, &["sync", "--allow-unsigned"]), "sync unsigned");
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn manages_repositories_and_keys() {
    let root = temp_root("config");
    let add = run(
        &root,
        &[
            "repo",
            "add",
            "file:///tmp/core",
            "--name",
            "core",
            "--branch",
            "edge",
        ],
    );
    assert_ok(&add, "repo add");
    let list = run(&root, &["repo", "list"]);
    assert_ok(&list, "repo list");
    assert!(String::from_utf8_lossy(&list.stdout).contains("core"));

    use ed25519_dalek::SigningKey;
    let key = SigningKey::from_bytes(&[3u8; 32]);
    let hex_key = hex::encode(key.verifying_key().to_bytes());
    assert_ok(&run(&root, &["key", "add", &hex_key]), "key add");
    let keys = run(&root, &["key", "list"]);
    assert_ok(&keys, "key list");
    assert!(String::from_utf8_lossy(&keys.stdout).contains(&hex_key[..16]));

    assert_ok(&run(&root, &["repo", "remove", "core"]), "repo remove");
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn builds_source_recipe_and_installs_result() {
    let root = temp_root("build");
    let source = root.join("source");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("message.txt"), b"from source\n").unwrap();
    let recipe = root.join("hello.toml");
    std::fs::write(
        &recipe,
        r#"
[package]
name = "hellopkg"
version = "1.0"
description = "Built from source"

[source]
path = "source"

[build]
script = "mkdir -p $DESTDIR/usr/share/hello && cp message.txt $DESTDIR/usr/share/hello/"
"#,
    )
    .unwrap();

    let path = recipe.to_string_lossy();
    assert_ok(
        &run(&root, &["build", &path, "--install", "-y"]),
        "build and install",
    );
    assert_eq!(
        std::fs::read_to_string(root.join("usr/share/hello/message.txt")).unwrap(),
        "from source\n"
    );
    let verify = run(&root, &["verify"]);
    assert_ok(&verify, "verify built package");
    std::fs::remove_dir_all(root).ok();
}
