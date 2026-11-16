//! Tests for this fork's defaults: fixed target and build directories and
//! the build-dir garbage collector. The rest of the testsuite runs with
//! `__CARGO_FORK_DEFAULTS_OPT_OUT=1`; these tests remove it.

use std::fs;
use std::path::{Path, PathBuf};

use crate::prelude::*;
use cargo_test_support::registry::Package;
use cargo_test_support::{
    Execs, Project, basic_lib_manifest, basic_manifest, git, paths, project, str,
};

fn fork_cargo(p: &Project, args: &str) -> Execs {
    let mut execs = p.cargo(args);
    execs.env_remove("__CARGO_FORK_DEFAULTS_OPT_OUT");
    execs
}

fn worktree_config_repo(extension: bool, worktree_config: Option<&str>) -> Project {
    let p = git::new("foo", |p| {
        p.file("src/lib.rs", "pub fn foo() {}")
            .file(".gitignore", "ignored.txt\n")
    });
    fs::write(p.root().join("ignored.txt"), "not a build input").unwrap();
    let config = p.root().join(".git/config");
    let mut contents = fs::read_to_string(&config).unwrap();
    contents.push_str(&format!(
        "\n[core]\n\tbare = true\n[extensions]\n\tworktreeConfig = {extension}\n"
    ));
    fs::write(config, contents).unwrap();
    if let Some(contents) = worktree_config {
        fs::write(p.root().join(".git/config.worktree"), contents).unwrap();
    }
    p
}

fn git_is_bare(root: &Path, bare: bool) -> Execs {
    let mut git = cargo_test_support::git_process("rev-parse --is-bare-repository");
    git.cwd(root);
    let mut execs = cargo_test_support::execs().with_process_builder(git);
    execs.with_stdout_data(format!("{bare}\n").as_str());
    execs
}

fn assert_package_files(cargo: &mut Execs, source: &str) {
    cargo
        .with_stdout_contains("Cargo.toml")
        .with_stdout_contains(source)
        .with_stdout_does_not_contain("ignored.txt")
        .run();
}

#[cargo_test]
fn git_worktree_config_overrides_bare() {
    let p = worktree_config_repo(true, Some("[core]\n\tbare = false\n"));
    git_is_bare(&p.root(), false).run();
    assert_package_files(
        &mut fork_cargo(&p, "package --list --allow-dirty"),
        "src/lib.rs",
    );
    fork_cargo(&p, "build").run();
    fork_cargo(&p, "check").run();
    fs::write(p.root().join("ignored.txt"), "still not a build input").unwrap();
    fork_cargo(&p, "build")
        .with_stderr_data(str![[r#"
[FINISHED] `dev` profile [unoptimized + debuginfo] target(s) in [ELAPSED]s

"#]])
        .run();
}

#[cargo_test]
fn git_worktree_config_normal_repository() {
    let p = git::new("foo", |p| {
        p.file("src/lib.rs", "pub fn foo() {}")
            .file(".gitignore", "ignored.txt\n")
    });
    git_is_bare(&p.root(), false).run();
    assert_package_files(
        &mut fork_cargo(&p, "package --list --allow-dirty"),
        "src/lib.rs",
    );
    fork_cargo(&p, "build").run();
    fork_cargo(&p, "check").run();
    fs::write(p.root().join("ignored.txt"), "still not a build input").unwrap();
    fork_cargo(&p, "build")
        .with_stderr_data(str![[r#"
[FINISHED] `dev` profile [unoptimized + debuginfo] target(s) in [ELAPSED]s

"#]])
        .run();
}

#[cargo_test]
fn git_worktree_config_bare_without_override() {
    let p = worktree_config_repo(true, None);
    git_is_bare(&p.root(), true).run();
    for command in ["build", "check"] {
        fork_cargo(&p, command)
            .with_status(101)
            .with_stderr_contains("[..]did not expect repo at [..] to be bare")
            .run();
    }
}

#[cargo_test]
fn git_worktree_config_disabled() {
    let p = worktree_config_repo(false, Some("[core]\n\tbare = false\n"));
    git_is_bare(&p.root(), true).run();
    fork_cargo(&p, "build")
        .with_status(101)
        .with_stderr_contains("[..]did not expect repo at [..] to be bare")
        .run();
}

#[cargo_test]
fn git_worktree_config_still_bare() {
    let p = worktree_config_repo(true, Some("[core]\n\tbare = true\n"));
    git_is_bare(&p.root(), true).run();
    fork_cargo(&p, "build")
        .with_status(101)
        .with_stderr_contains("[..]did not expect repo at [..] to be bare")
        .run();
}

#[cargo_test]
fn git_worktree_config_makes_repository_bare() {
    let p = worktree_config_repo(true, Some("[core]\n\tbare = true\n"));
    let config = p.root().join(".git/config");
    let contents = fs::read_to_string(&config).unwrap() + "\n[core]\n\tbare = false\n";
    fs::write(config, contents).unwrap();
    git_is_bare(&p.root(), true).run();
    for command in ["build", "check"] {
        fork_cargo(&p, command)
            .with_status(101)
            .with_stderr_contains("[..]did not expect repo at [..] to be bare")
            .run();
    }
}

#[cargo_test]
fn git_worktree_config_linked_worktree() {
    let (p, wt) = repo_with_worktree("fn main() { bar::bar(); }");
    let git_dir = git2::Repository::open(&wt).unwrap().path().to_path_buf();
    let config = p.root().join(".git/config");
    let original = fs::read_to_string(&config).unwrap();
    let configure = |extension| {
        fs::write(
            &config,
            format!(
                "{original}\n[core]\n\tbare = true\n[extensions]\n\tworktreeConfig = {extension}\n"
            ),
        )
        .unwrap();
    };
    configure(false);
    git_is_bare(&wt, false).run();
    fork_cargo(&p, "build").cwd(&wt).run();
    fork_cargo(&p, "check").cwd(&wt).run();

    // Enabling the extension makes the common bare value apply to linked
    // worktrees too, unless overridden in their own config.worktree.
    configure(true);
    git_is_bare(&wt, true).run();
    fork_cargo(&p, "build")
        .cwd(&wt)
        .with_status(101)
        .with_stderr_contains("[..]did not expect repo at [..] to be bare")
        .run();

    fs::write(git_dir.join("config.worktree"), "[core]\n\tbare = false\n").unwrap();
    git_is_bare(&wt, false).run();
    fork_cargo(&p, "build").cwd(&wt).run();
    fork_cargo(&p, "check").cwd(&wt).run();

    fs::write(git_dir.join("config.worktree"), "[core]\n\tbare = true\n").unwrap();
    git_is_bare(&wt, true).run();
    fork_cargo(&p, "build")
        .cwd(&wt)
        .with_status(101)
        .with_stderr_contains("[..]did not expect repo at [..] to be bare")
        .run();
}

#[cargo_test]
fn git_worktree_config_explicit_worktree() {
    let p = worktree_config_repo(true, Some("[core]\n\tbare = false\n\tworktree = ..\n"));
    git_is_bare(&p.root(), false).run();
    assert_package_files(
        &mut fork_cargo(&p, "package --list --allow-dirty"),
        "src/lib.rs",
    );
    // A gitfile pointing at a separate Git directory must not make Cargo use
    // the Git directory's parent as the worktree.
    let git_dir = paths::root().join("separate.git");
    fs::rename(p.root().join(".git"), &git_dir).unwrap();
    fs::write(
        p.root().join(".git"),
        format!("gitdir: {}\n", git_dir.display()),
    )
    .unwrap();
    fs::write(
        git_dir.join("config.worktree"),
        "[core]\n\tbare = false\n\tworktree = ../foo\n",
    )
    .unwrap();
    git_is_bare(&p.root(), false).run();
    assert_package_files(
        &mut fork_cargo(&p, "package --list --allow-dirty"),
        "src/lib.rs",
    );
    fork_cargo(&p, "build").run();
    fork_cargo(&p, "check").run();
    fs::write(p.root().join("ignored.txt"), "still not a build input").unwrap();
    fork_cargo(&p, "build")
        .with_stderr_data(str![[r#"
[FINISHED] `dev` profile [unoptimized + debuginfo] target(s) in [ELAPSED]s

"#]])
        .run();
}

#[cargo_test]
fn git_worktree_config_submodule() {
    let p = git::new("foo", |p| p.file("src/lib.rs", "pub fn foo() {}"));
    let sub = git::new("bar", |p| p.file("src/lib.rs", "pub fn bar() {}"));
    let repository = git2::Repository::open(p.root()).unwrap();
    git::add_submodule(&repository, sub.root().to_url().as_ref(), Path::new("sub"));
    git::commit(&repository);
    let mut git = cargo_test_support::git_process("submodule absorbgitdirs sub");
    git.cwd(p.root());
    cargo_test_support::execs().with_process_builder(git).run();
    let root = p.root().join("sub");
    let git_dir = git2::Repository::open(&root).unwrap().path().to_path_buf();
    let config = git_dir.join("config");
    let contents = fs::read_to_string(&config).unwrap()
        + "\n[core]\n\tbare = true\n\tworktree = ../../../sub\n[extensions]\n\tworktreeConfig = true\n";
    fs::write(config, contents).unwrap();
    fs::write(git_dir.join("config.worktree"), "[core]\n\tbare = false\n").unwrap();
    git_is_bare(&root, false).run();
    fork_cargo(&p, "build").cwd(&root).run();
    fork_cargo(&p, "check").cwd(&root).run();
    assert_package_files(
        fork_cargo(&p, "package --list --allow-dirty").cwd(&root),
        "src/lib.rs",
    );
}

#[cargo_test]
fn git_worktree_config_environment_worktree() {
    let p = worktree_config_repo(true, Some("[core]\n\tbare = true\n"));
    git_is_bare(&p.root(), false)
        .env("GIT_WORK_TREE", p.root())
        .run();
    for command in ["build", "check"] {
        fork_cargo(&p, command).env("GIT_WORK_TREE", p.root()).run();
    }
    assert_package_files(
        fork_cargo(&p, "package --list --allow-dirty").env("GIT_WORK_TREE", p.root()),
        "src/lib.rs",
    );
}

#[cargo_test]
fn git_worktree_config_environment_linked_worktree() {
    let (p, wt) = repo_with_worktree("fn main() { bar::bar(); }");
    let config = p.root().join(".git/config");
    let contents = fs::read_to_string(&config).unwrap()
        + "\n[core]\n\tbare = true\n[extensions]\n\tworktreeConfig = true\n";
    fs::write(config, contents).unwrap();
    git_is_bare(&wt, false).env("GIT_WORK_TREE", &wt).run();
    for command in ["build", "check"] {
        fork_cargo(&p, command)
            .cwd(&wt)
            .env("GIT_WORK_TREE", &wt)
            .run();
    }
    assert_package_files(
        fork_cargo(&p, "package --list --allow-dirty")
            .cwd(&wt)
            .env("GIT_WORK_TREE", &wt),
        "src/main.rs",
    );
}

#[cargo_test]
fn git_worktree_config_environment_relative_worktree() {
    let p = worktree_config_repo(true, Some("[core]\n\tbare = false\n"));
    git_is_bare(&p.root(), false)
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.worktree")
        .env("GIT_CONFIG_VALUE_0", ".")
        .run();
    for command in ["build", "check", "package --list --allow-dirty"] {
        let mut cargo = fork_cargo(&p, command);
        cargo
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "core.worktree")
            .env("GIT_CONFIG_VALUE_0", ".");
        if command.starts_with("package") {
            assert_package_files(&mut cargo, "src/lib.rs");
        } else {
            cargo.run();
        }
    }
}

#[cargo_test]
fn git_worktree_config_empty_worktree() {
    let p = worktree_config_repo(true, Some("[core]\n\tbare = false\n\tworktree = \"\"\n"));
    let mut git = cargo_test_support::git_process("rev-parse --is-bare-repository");
    git.cwd(p.root());
    cargo_test_support::execs()
        .with_process_builder(git)
        .with_status(128)
        .with_stderr_contains("fatal: cannot chdir to ''[..]")
        .run();
    fork_cargo(&p, "build")
        .with_status(101)
        .with_stderr_contains("[..]core.worktree[..]")
        .run();
}

#[cargo_test]
fn git_worktree_config_self_disabling() {
    let p = worktree_config_repo(
        true,
        Some("[extensions]\n\tworktreeConfig = false\n[core]\n\tbare = false\n"),
    );
    git_is_bare(&p.root(), false).run();
    fork_cargo(&p, "build").run();
    assert_package_files(
        &mut fork_cargo(&p, "package --list --allow-dirty"),
        "src/lib.rs",
    );
    let config = p.root().join(".git/config");
    let contents = fs::read_to_string(&config).unwrap() + "\n[core]\n\tbare = false\n";
    fs::write(config, contents).unwrap();
    fs::write(
        p.root().join(".git/config.worktree"),
        "[extensions]\n\tworktreeConfig = false\n[core]\n\tbare = true\n",
    )
    .unwrap();
    git_is_bare(&p.root(), true).run();
    fork_cargo(&p, "build")
        .with_status(101)
        .with_stderr_contains("[..]did not expect repo at [..] to be bare")
        .run();
}

#[cargo_test]
fn git_worktree_config_symlinked_git_dir() {
    if !cargo_test_support::symlink_supported() {
        return;
    }
    let p = worktree_config_repo(true, Some("[core]\n\tbare = false\n\tworktree = ../..\n"));
    fs::create_dir(p.root().join("metadata")).unwrap();
    fs::rename(p.root().join(".git"), p.root().join("metadata/repo.git")).unwrap();
    p.symlink("metadata/repo.git", ".git");
    fs::write(p.root().join(".gitignore"), "ignored.txt\nmetadata/\n").unwrap();
    git_is_bare(&p.root(), false).run();
    assert_package_files(
        &mut fork_cargo(&p, "package --list --allow-dirty"),
        "src/lib.rs",
    );
    fork_cargo(&p, "build").run();
    fork_cargo(&p, "check").run();
}

#[cargo_test]
fn git_worktree_config_symlinked_ancestor() {
    if !cargo_test_support::symlink_supported() {
        return;
    }
    let p = worktree_config_repo(true, Some("[core]\n\tbare = false\n\tworktree = ..\n"));
    let alias = paths::root().join("alias");
    p.symlink(p.root(), &alias);
    git_is_bare(&alias, false).run();
    let manifest = alias.join("Cargo.toml");
    assert_package_files(
        fork_cargo(&p, "package --list --allow-dirty --manifest-path").arg(&manifest),
        "src/lib.rs",
    );
    fork_cargo(&p, "build --manifest-path").arg(&manifest).run();
    fork_cargo(&p, "check --manifest-path").arg(&manifest).run();
}

#[cargo_test]
fn git_worktree_config_gitfile() {
    let p = worktree_config_repo(true, Some("[core]\n\tbare = false\n"));
    let git_dir = paths::root().join("separate.git");
    fs::rename(p.root().join(".git"), &git_dir).unwrap();
    fs::write(
        p.root().join(".git"),
        format!("gitdir: {}\n", git_dir.display()),
    )
    .unwrap();
    git_is_bare(&p.root(), false).run();
    assert_package_files(
        &mut fork_cargo(&p, "package --list --allow-dirty"),
        "src/lib.rs",
    );
    fork_cargo(&p, "build").run();
    fork_cargo(&p, "check").run();
}

/// The shared build-dir of the `foo` project.
fn shared_build_dir() -> PathBuf {
    let targets = paths::cargo_home().join("targets");
    let mut dirs = fs::read_dir(&targets)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.file_name().unwrap().to_str().unwrap().starts_with("foo-"))
        .collect::<Vec<_>>();
    assert_eq!(dirs.len(), 1, "{dirs:?}");
    dirs.pop().unwrap()
}

// A symlink spelling of CARGO_HOME also aliases targets/shared-deps, so the
// test distinguishes literal source paths, not just different physical caches.
pub(super) fn source_path_fixture() -> (Project, PathBuf) {
    let source = r#"include!(concat!(env!("OUT_DIR"), "/generated.rs"));"#;
    let script = r#"
        fn main() {
            use std::{env, fs, path::PathBuf};
            let root = env::var("CARGO_MANIFEST_DIR").unwrap();
            let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
            fs::write(out.join("generated.rs"), format!("pub const SOURCE: &str = {root:?};")).unwrap();
            println!("cargo:rerun-if-changed=build.rs");
        }
    "#;
    Package::new("registry_probe", "1.0.0")
        .file("src/lib.rs", source)
        .file("build.rs", script)
        .publish();
    let git = git::new("git_probe", |p| {
        p.file("Cargo.toml", &basic_lib_manifest("git_probe"))
            .file("src/lib.rs", source)
            .file("build.rs", script)
    });
    let p = project()
        .file(
            "Cargo.toml",
            &format!(
                "[package]\nname='foo'\nversion='0.1.0'\nedition='2024'\n\
                 [dependencies]\nregistry_probe='1'\ngit_probe={{git={:?}}}\n",
                git.url().as_str()
            ),
        )
        .file(
            "src/main.rs",
            r#"fn main() { println!("{}\n{}", registry_probe::SOURCE, git_probe::SOURCE); }"#,
        )
        .build();
    let alias = paths::root().join("alias-home");
    p.symlink(paths::cargo_home(), &alias);
    (p, alias)
}

fn shared_source_unit_dirs(package: &str) -> std::collections::BTreeSet<PathBuf> {
    fs::read_dir(
        paths::cargo_home()
            .join("targets/shared-deps/debug/build")
            .join(package),
    )
    .unwrap()
    .map(|entry| entry.unwrap().path())
    .filter(|path| path.is_dir())
    .collect()
}

#[cargo_test]
fn shared_units_distinguish_source_path_spellings() {
    if !cargo_test_support::symlink_supported() {
        return;
    }
    let (p, alias) = source_path_fixture();
    let home = paths::cargo_home();
    let build = |home: &Path, fresh: bool| {
        let mut cargo = fork_cargo(&p, "run -vv");
        cargo
            .env("CARGO_HOME", home)
            .with_stderr_does_not_contain("[..]the path to the source changed[..]");
        if fresh {
            cargo
                .with_stderr_contains("[FRESH] registry_probe v1.0.0")
                .with_stderr_contains("[FRESH] git_probe v0.5.0 ([..])")
                .with_stderr_does_not_contain("[COMPILING] registry_probe [..]")
                .with_stderr_does_not_contain("[COMPILING] git_probe [..]");
        }
        let output = cargo.run();
        // Both build scripts must have run with their own spelling, not replayed
        // an absolute source path or OUT_DIR generated by the other build.
        let stdout = String::from_utf8(output.stdout).unwrap();
        let paths = stdout
            .lines()
            .filter(|line| Path::new(line).is_absolute())
            .collect::<Vec<_>>();
        assert_eq!(paths.len(), 2, "{stdout}");
        for line in paths {
            assert!(Path::new(line).starts_with(home), "{line}");
        }
    };
    build(&home, false);
    let first = ["registry_probe", "git_probe"].map(shared_source_unit_dirs);
    build(&alias, false);
    let both = ["registry_probe", "git_probe"].map(shared_source_unit_dirs);
    for (first, both) in first.iter().zip(&both) {
        // Library, compiled build script, and executed build script.
        assert_eq!(first.len(), 3, "{first:?}");
        assert_eq!(both.len(), 2 * first.len(), "{both:?}");
        assert!(first.is_subset(both));
    }
    for home in [&home, &alias, &home, &alias] {
        build(home, true);
    }
    assert_eq!(
        both,
        ["registry_probe", "git_probe"].map(shared_source_unit_dirs)
    );
}

pub(super) fn vendored_source_fixture() -> (Project, PathBuf, PathBuf) {
    Package::new("bar", "1.0.0")
        .file("src/lib.rs", "pub fn bar() -> Option<()> { None }")
        .publish();
    let p = project()
        .file(
            "Cargo.toml",
            &(basic_manifest("foo", "0.1.0") + "\n[dependencies]\nbar='1'\n"),
        )
        .file("src/main.rs", "fn main() { let _ = bar::bar(); }")
        .build();
    let vendor = paths::root().join("vendor");
    let alias = paths::root().join("vendor-alias");
    fork_cargo(&p, "vendor --respect-source-config")
        .arg(&vendor)
        .run();
    p.symlink(&vendor, &alias);
    (p, vendor, alias)
}

pub(super) fn vendored_source_cargo(p: &Project, args: &str, source: &Path) -> Execs {
    let mut cargo = fork_cargo(p, args);
    cargo
        .arg("--config=source.crates-io.replace-with='vendored'")
        .arg(format!(
            "--config=source.vendored.directory={:?}",
            source.to_str().unwrap()
        ));
    cargo
}

#[cargo_test]
fn shared_units_distinguish_source_paths_in_one_home() {
    if !cargo_test_support::symlink_supported() {
        return;
    }
    let (p, vendor, alias) = vendored_source_fixture();
    let build = |source: &Path, fresh: bool| {
        let mut cargo = vendored_source_cargo(&p, "build -vv", source);
        cargo.with_stderr_does_not_contain("[..]the path to the source changed[..]");
        if fresh {
            cargo
                .with_stderr_contains("[FRESH] bar v1.0.0")
                .with_stderr_does_not_contain("[COMPILING] bar [..]");
        }
        cargo.run();
    };
    // Only the source spelling changes: home, pool, sysroot and flags stay
    // identical. Hashing CARGO_HOME or canonicalizing the source is not enough.
    build(&vendor, false);
    let first = shared_source_unit_dirs("bar");
    build(&alias, false);
    assert_eq!(shared_source_unit_dirs("bar").len(), 2 * first.len());
    for source in [&vendor, &alias] {
        build(source, true);
    }
}

#[cargo_test]
fn shared_units_hash_flags_even_with_remap() {
    Package::new("bar", "1.0.0")
        .file("src/lib.rs", "pub fn bar() {}")
        .publish();
    let p = project()
        .file("Cargo.toml", &basic_manifest("foo", "0.1.0"))
        .file("src/main.rs", "fn main() { bar::bar(); }")
        .build();
    p.change_file(
        "Cargo.toml",
        &(p.read_file("Cargo.toml") + "\n[dependencies]\nbar='1'\n"),
    );
    let build = |flags: &str, fresh: bool| {
        let mut cargo = fork_cargo(&p, "build -v");
        cargo.env("RUSTFLAGS", flags);
        if fresh {
            cargo
                .with_stderr_contains("[FRESH] bar v1.0.0")
                .with_stderr_does_not_contain("[COMPILING] [..]");
        }
        cargo.run();
    };
    let flags = [
        "--remap-path-prefix=/first=/src -C debuginfo=0",
        "--remap-path-prefix=/second=/src -C debuginfo=1",
    ];
    build(flags[0], false);
    let first = shared_source_unit_dirs("bar");
    build(flags[1], false);
    assert_eq!(shared_source_unit_dirs("bar").len(), 2 * first.len());
    for flags in flags {
        build(flags, true);
    }
}

#[cargo_test]
fn shared_docs_distinguish_source_paths() {
    if !cargo_test_support::symlink_supported() {
        return;
    }
    let (p, vendor, alias) = vendored_source_fixture();
    for source in [&vendor, &alias, &vendor, &alias] {
        vendored_source_cargo(&p, "doc -p bar --no-deps -vv", source)
            .with_stderr_does_not_contain("[..]the path to the source changed[..]")
            .run();
        vendored_source_cargo(&p, "doc -p bar --no-deps -vv", source)
            .with_stderr_contains("[FRESH] bar v1.0.0")
            .with_stderr_does_not_contain("[DOCUMENTING] [..]")
            .run();
    }
    assert_eq!(shared_source_unit_dirs("bar").len(), 2);
}

#[cargo_test]
fn shared_docs_hash_rustdocflags_even_with_remap() {
    Package::new("bar", "1.0.0")
        .file("src/lib.rs", "pub fn bar() {}")
        .publish();
    let p = project()
        .file(
            "Cargo.toml",
            &(basic_manifest("foo", "0.1.0") + "\n[dependencies]\nbar='1'\n"),
        )
        .file("src/main.rs", "fn main() {}")
        .build();
    let headers = ["first", "second"].map(|name| {
        let path = p.root().join(format!("{name}.html"));
        fs::write(&path, format!("<!-- {name}-header -->")).unwrap();
        (path, format!("<!-- {name}-header -->"))
    });
    for i in [0, 1, 0, 1] {
        let flags = format!(
            "--remap-path-prefix=/spell-{i}=/src --html-in-header={}",
            headers[i].0.display()
        );
        for fresh in [false, true] {
            let mut cargo = fork_cargo(&p, "doc -p bar --no-deps -vv");
            cargo.env("RUSTDOCFLAGS", &flags);
            if fresh {
                cargo
                    .with_stderr_contains("[FRESH] bar v1.0.0")
                    .with_stderr_does_not_contain("[DOCUMENTING] [..]");
            }
            cargo.run();
            assert!(
                p.read_file("target/doc/bar/index.html")
                    .contains(&headers[i].1)
            );
        }
    }
    assert_eq!(shared_source_unit_dirs("bar").len(), 2);
}

#[cargo_test]
fn shared_docs_json_has_its_own_unit_directory() {
    Package::new("bar", "1.0.0")
        .file("src/lib.rs", "pub fn bar() {}")
        .publish();
    let p = project()
        .file(
            "Cargo.toml",
            &(basic_manifest("foo", "0.1.0") + "\n[dependencies]\nbar='1'\n"),
        )
        .file("src/main.rs", "fn main() {}")
        .build();
    for json in [false, true, false, true] {
        for fresh in [false, true] {
            let mut cargo = fork_cargo(&p, "doc -p bar --no-deps -vv");
            if json {
                cargo
                    .arg("--output-format=json")
                    .arg("-Zunstable-options")
                    .masquerade_as_nightly_cargo(&["rustdoc-output-format"])
                    .env("RUSTC_BOOTSTRAP", "1");
            }
            if fresh {
                cargo
                    .with_stderr_contains("[FRESH] bar v1.0.0")
                    .with_stderr_does_not_contain("[DOCUMENTING] [..]");
            }
            cargo.run();
            if json {
                let document: serde_json::Value =
                    serde_json::from_str(&p.read_file("target/doc/bar.json")).unwrap();
                assert!(document["index"].is_object());
            } else {
                assert!(p.root().join("target/doc/bar/index.html").is_file());
            }
        }
    }
    assert_eq!(shared_source_unit_dirs("bar").len(), 2);
}

#[cargo_test]
fn shared_docs_hash_extern_map_and_keep_html_current() {
    if !cargo_test_support::symlink_supported() {
        return;
    }
    Package::new("baz", "1.0.0")
        .file("src/lib.rs", "pub struct Baz;")
        .publish();
    Package::new("bar", "1.0.0")
        .dep("baz", "1")
        .file(
            "src/lib.rs",
            "pub struct Bar; pub fn bar() -> Option<baz::Baz> { None }",
        )
        .publish();
    let p = project()
        .file(
            "Cargo.toml",
            &(basic_lib_manifest("foo") + "\n[dependencies]\nbar='1'\n"),
        )
        .file("src/lib.rs", "pub fn foo() -> Option<bar::Bar> { None }")
        .build();
    let docs = paths::root().join("docs");
    let alias = paths::root().join("docs-alias");
    fs::create_dir(&docs).unwrap();
    p.symlink(&docs, &alias);
    let urls = [&docs, &alias].map(|path| url::Url::from_directory_path(path).unwrap().to_string());
    // Check both shared dependency docs and workspace path docs. Repeating the
    // same matrix with upstream-compatible layout must keep HTML current too.
    for (package, dependency, item) in [("bar", "baz", "Baz"), ("foo", "bar", "Bar")] {
        // Existing local dependency docs take precedence over extern roots.
        // Each package case starts without the previous case's HTML.
        let doc_dir = p.root().join("target/doc");
        if doc_dir.exists() {
            fs::remove_dir_all(doc_dir).unwrap();
        }
        for shared in [true, false] {
            let mut units_seen = std::collections::BTreeSet::new();
            for (std, registry) in [(0, 0), (1, 0), (1, 1), (0, 0)] {
                for fresh in [false, true] {
                    let mut cargo = fork_cargo(&p, "doc --lib --no-deps -vv -Zrustdoc-map");
                    cargo
                        .arg("-p")
                        .arg(package)
                        .masquerade_as_nightly_cargo(&["rustdoc-map"])
                        .env("RUSTC_BOOTSTRAP", "1")
                        .env("RUSTDOCFLAGS", "--extern-html-root-takes-precedence")
                        .arg(format!("--config=doc.extern-map.std={:?}", urls[std]))
                        .arg(format!(
                            "--config=doc.extern-map.registries.crates-io={:?}",
                            urls[registry]
                        ));
                    if shared {
                        cargo.with_stderr_does_not_contain("[..]the config settings changed[..]");
                    } else {
                        cargo
                            .env("__CARGO_FORK_DEFAULTS_OPT_OUT", "1")
                            .env("CARGO_SHARED_DEPS", "0")
                            .env("CARGO_SHARE_WORKSPACE_UNITS", "0");
                    }
                    if fresh {
                        cargo
                            .with_stderr_contains(format!("[FRESH] {package} v[..]"))
                            .with_stderr_does_not_contain("[DOCUMENTING] [..]");
                    }
                    cargo.run();
                    if shared {
                        // Workspace GC may evict old revisions. Count actual
                        // unit directories as they are used, not GC lock files.
                        let root = if package == "foo" {
                            shared_build_dir()
                        } else {
                            paths::cargo_home().join("targets/shared-deps")
                        };
                        units_seen.extend(
                            fs::read_dir(root.join("debug/build").join(package))
                                .unwrap()
                                .map(|entry| entry.unwrap().path())
                                .filter(|path| path.is_dir()),
                        );
                    }
                    let html = p.read_file(&format!("target/doc/{package}/fn.{package}.html"));
                    // Explicit precedence makes rustdoc honor the configured
                    // std root rather than the sysroot's html_root_url attribute.
                    assert!(
                        html.contains(&format!("{}core/option/enum.Option.html", urls[std])),
                        "{html}"
                    );
                    assert!(
                        html.contains(&format!(
                            "{}{dependency}/1.0.0/{dependency}/struct.{item}.html",
                            urls[registry]
                        )),
                        "{html}"
                    );
                }
            }
            if shared {
                assert_eq!(
                    units_seen.len(),
                    3,
                    "std and registry map values must both distinguish doc units"
                );
            }
        }
    }
}

#[cargo_test]
fn target_dir_cannot_be_moved() {
    let p = project()
        .file("src/main.rs", "fn main() {}")
        .file(
            ".cargo/config.toml",
            r#"
            [build]
            build-dir = "config-build-dir"
            "#,
        )
        .build();

    fork_cargo(&p, "build --target-dir flag-dir")
        .env("CARGO_TARGET_DIR", "env-dir")
        .with_stderr_data(str![[r#"
[WARNING] ignoring the configured target directory (--target-dir, CARGO_TARGET_DIR, build.target-dir); this cargo always uses `./target`
[WARNING] ignoring the configured build directory (build.build-dir); this cargo always uses `~/.cargo/targets`
[COMPILING] foo v0.0.1 ([ROOT]/foo)
[FINISHED] `dev` profile [unoptimized + debuginfo] target(s) in [ELAPSED]s

"#]])
        .run();

    assert!(p.bin("foo").is_file());
    for dir in ["flag-dir", "env-dir", "config-build-dir"] {
        assert!(!p.root().join(dir).exists(), "{dir} was created");
    }
    assert!(shared_build_dir().join("debug/build").is_dir());
    assert!(!p.root().join("target/debug/build").exists());
}

#[cargo_test]
fn gc_keeps_two_incremental_dirs_per_crate() {
    let p = project().file("src/main.rs", "fn main() {}").build();
    fork_cargo(&p, "build").run();

    let incremental = shared_build_dir().join("debug/incremental");
    for id in ["a", "b", "c", "d"] {
        let dir = incremental.join(format!("bar-{id}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("s-session"), "").unwrap();
    }
    fork_cargo(&p, "build")
        .env("CARGO_BUILD_GC_FREQUENCY", "always")
        .run();

    let left = fs::read_dir(&incremental)
        .unwrap()
        .filter(|e| {
            let name = e.as_ref().unwrap().file_name();
            name.to_str().unwrap().starts_with("bar-")
        })
        .count();
    assert_eq!(left, 2);
}

#[cargo_test]
fn gc_records_build_sizes() {
    let p = project().file("src/main.rs", "fn main() {}").build();
    fork_cargo(&p, "build").run();
    fork_cargo(&p, "build").run();

    let sizes = fs::read_to_string(shared_build_dir().join(".cargo-gc-sizes")).unwrap();
    assert_eq!(sizes.lines().count(), 2, "{sizes}");
}

#[cargo_test]
fn gc_keeps_build_script_outputs_above_out_dir() {
    let p = project()
        .file(
            "build.rs",
            r#"
            use std::{env, fs, path::PathBuf};

            fn main() {
                println!("cargo:rerun-if-changed=build.rs");
                let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
                // rusty_v8 walks up three levels to write its native archive.
                let obj = out.ancestors().nth(3).unwrap().join("gn_out/obj");
                fs::create_dir_all(&obj).unwrap();
                let generated = obj.join("generated.txt");
                fs::write(&generated, "generated by build.rs").unwrap();
                println!("cargo:rustc-env=GENERATED={}", generated.display());
            }
            "#,
        )
        .file(
            "src/lib.rs",
            r#"
            #[test]
            fn generated_file_is_available() {
                assert_eq!(include_str!(env!("GENERATED")), "generated by build.rs");
            }
            "#,
        )
        .build();

    // The check runs the build script and GC. The test reuses the script's output.
    fork_cargo(&p, "check").env("CARGO_SHARED_DEPS", "0").run();
    fork_cargo(&p, "test").env("CARGO_SHARED_DEPS", "0").run();
}

#[cargo_test]
fn gc_keeps_native_archive_of_build_script_under_pressure() {
    // Same scenario as huacnlee/cargo#12: a build script puts a native archive
    // where rusty_v8 does, three parents above `OUT_DIR`, and a test target
    // links it before a library target does, both under GC pressure. The
    // build script stays dependency-free and offline. The run count lives in
    // the project, because anything inside the build directory could be
    // collected together with the evidence of its removal.
    let p = project()
        .file(
            "build.rs",
            r#"
            use std::{env, fs, path::PathBuf};

            fn main() {
                println!("cargo:rerun-if-changed=build.rs");
                let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
                let count = manifest.join("build-count");
                let runs: u32 = fs::read_to_string(&count)
                    .unwrap_or_default().parse().unwrap_or(0);
                fs::write(count, (runs + 1).to_string()).unwrap();

                let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
                let native = out.ancestors().nth(3).unwrap().join("gn_out/obj");
                fs::create_dir_all(&native).unwrap();
                let archive = if cfg!(windows) { "native_probe.lib" } else { "libnative_probe.a" };
                fs::write(native.join(archive), b"!<arch>\n").unwrap();
                fs::write(manifest.join("native-dir"), native.to_str().unwrap()).unwrap();
                println!("cargo:rustc-link-search=native={}", native.display());
                // link.exe rejects this placeholder archive (LNK1107), and
                // writing a valid COFF archive is not worth it: what is
                // tested is where the archive lives and that GC keeps it,
                // not the link. Unix, where the fork ships, still links it.
                if !cfg!(windows) {
                    println!("cargo:rustc-link-lib=static=native_probe");
                }
            }
            "#,
        )
        .file("src/lib.rs", "#[test] fn t() {}")
        .build();
    let pressure = |args: &str| {
        fork_cargo(&p, args)
            .env("CARGO_BUILD_GC_FREQUENCY", "always")
            .env("CARGO_BUILD_GC_MAX_SIZE", "1 KiB")
            .run();
    };
    let archive = if cfg!(windows) {
        "native_probe.lib"
    } else {
        "libnative_probe.a"
    };

    // The test target links the archive before post-build GC runs.
    pressure("test --lib --no-run");
    let native = PathBuf::from(fs::read_to_string(p.root().join("native-dir")).unwrap());
    // The archive belongs to the build script's run unit, so the unit's lock
    // and the collector cover it.
    assert!(
        native.starts_with(shared_build_dir().join("debug/build")),
        "{native:?}"
    );
    assert!(
        native
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("fingerprint")
            .is_dir()
    );
    assert!(native.join(archive).is_file());

    // The library target links the same archive without running the script
    // again.
    pressure("build --lib");
    assert!(native.join(archive).is_file());
    assert_eq!(
        fs::read_to_string(p.root().join("build-count")).unwrap(),
        "1"
    );
}

#[cargo_test]
fn check_and_build_share_registry_dependencies() {
    Package::new("bar", "1.0.0")
        .file("src/lib.rs", "pub fn bar() {}")
        .publish();
    let p = project()
        .file(
            "Cargo.toml",
            r#"
            [package]
            name = "foo"
            version = "0.1.0"
            edition = "2015"

            [dependencies]
            bar = "1.0"
            "#,
        )
        .file("src/main.rs", "fn main() { bar::bar(); }")
        .build();

    // `bar` is built, not only checked, so the build below reuses it.
    fork_cargo(&p, "check")
        .with_stderr_data(str![[r#"
[UPDATING] `dummy-registry` index
[LOCKING] 1 package to highest compatible version
[DOWNLOADING] crates ...
[DOWNLOADED] bar v1.0.0 (registry `dummy-registry`)
[COMPILING] bar v1.0.0
[CHECKING] foo v0.1.0 ([ROOT]/foo)
[FINISHED] `dev` profile [unoptimized + debuginfo] target(s) in [ELAPSED]s

"#]])
        .run();
    fork_cargo(&p, "build")
        .with_stderr_data(str![[r#"
[COMPILING] foo v0.1.0 ([ROOT]/foo)
[FINISHED] `dev` profile [unoptimized + debuginfo] target(s) in [ELAPSED]s

"#]])
        .run();

    // Opting out checks it again.
    fork_cargo(&p, "check")
        .env("CARGO_CHECK_SHARED_DEPS", "0")
        .with_stderr_data(str![[r#"
[CHECKING] bar v1.0.0
[CHECKING] foo v0.1.0 ([ROOT]/foo)
[FINISHED] `dev` profile [unoptimized + debuginfo] target(s) in [ELAPSED]s

"#]])
        .run();
}

#[cargo_test]
fn registry_dependencies_of_a_patched_crate_are_checked() {
    Package::new("bar", "1.0.0")
        .file("src/lib.rs", "pub struct Bar;")
        .publish();
    Package::new("baz", "1.0.0")
        .dep("bar", "1.0")
        .file("src/lib.rs", "pub fn bar() -> bar::Bar { bar::Bar }")
        .publish();
    Package::new("qux", "1.0.0")
        .file("src/lib.rs", "pub fn qux() {}")
        .publish();
    let p = project()
        .file(
            "Cargo.toml",
            r#"
            [package]
            name = "foo"
            version = "0.1.0"
            edition = "2015"

            [dependencies]
            bar = "1.0"
            baz = "1.0"
            qux = "1.0"

            [patch.crates-io]
            bar = { path = "bar" }
            "#,
        )
        .file(
            "src/lib.rs",
            "pub fn bar() -> bar::Bar { qux::qux(); baz::bar() }",
        )
        .file("bar/Cargo.toml", &basic_manifest("bar", "1.0.0"))
        .file("bar/src/lib.rs", "pub struct Bar;")
        .build();

    // `baz` links the patched `bar`, so it is checked against the same `bar`
    // as `foo`; `qux` links nothing local and is still built.
    fork_cargo(&p, "check")
        .with_stderr_data(
            str![[r#"
[UPDATING] `dummy-registry` index
[LOCKING] 3 packages to highest compatible versions
[DOWNLOADING] crates ...
[DOWNLOADED] baz v1.0.0 (registry `dummy-registry`)
[DOWNLOADED] qux v1.0.0 (registry `dummy-registry`)
[CHECKING] bar v1.0.0 ([ROOT]/foo/bar)
[CHECKING] baz v1.0.0
[COMPILING] qux v1.0.0
[CHECKING] foo v0.1.0 ([ROOT]/foo)
[FINISHED] `dev` profile [unoptimized + debuginfo] target(s) in [ELAPSED]s

"#]]
            .unordered(),
        )
        .run();
    fork_cargo(&p, "build")
        .with_stderr_data(
            str![[r#"
[COMPILING] bar v1.0.0 ([ROOT]/foo/bar)
[COMPILING] baz v1.0.0
[COMPILING] foo v0.1.0 ([ROOT]/foo)
[FINISHED] `dev` profile [unoptimized + debuginfo] target(s) in [ELAPSED]s

"#]]
            .unordered(),
        )
        .run();
}

/// A git repository `foo` with a library member `bar`, and a worktree of it.
fn repo_with_worktree(main: &str) -> (Project, PathBuf) {
    let p = git::new("foo", |p| {
        p.file(
            "Cargo.toml",
            r#"
            [package]
            name = "foo"
            version = "0.1.0"
            edition = "2015"

            [dependencies]
            bar = { path = "bar" }

            [workspace]
            "#,
        )
        .file("src/main.rs", main)
        .file("bar/Cargo.toml", &basic_lib_manifest("bar"))
        .file("bar/src/lib.rs", "pub fn bar() -> u32 { 1 }")
        .file("bar/tests/t.rs", "#[test] fn t() {}")
    });
    let repo = git2::Repository::open(p.root()).unwrap();
    let worktree = paths::root().join("wt");
    repo.worktree("wt", &worktree, Some(&git2::WorktreeAddOptions::new()))
        .unwrap();
    (p, worktree)
}

#[cargo_test]
fn worktrees_share_workspace_units() {
    let (p, wt) = repo_with_worktree("fn main() { println!(\"{}\", bar::bar()); }");
    fork_cargo(&p, "build")
        .with_stderr_data(str![[r#"
[COMPILING] bar v0.5.0 ([ROOT]/foo/bar)
[COMPILING] foo v0.1.0 ([ROOT]/foo)
[FINISHED] `dev` profile [unoptimized + debuginfo] target(s) in [ELAPSED]s

"#]])
        .run();

    // Same files: nothing to build.
    fork_cargo(&p, "build")
        .cwd(&wt)
        .with_stderr_data(str![[r#"
[FINISHED] `dev` profile [unoptimized + debuginfo] target(s) in [ELAPSED]s

"#]])
        .run();

    // An edit in one worktree leaves the other one's units alone.
    fs::write(wt.join("bar/src/lib.rs"), "pub fn bar() -> u32 { 2 }").unwrap();
    fork_cargo(&p, "run")
        .cwd(&wt)
        .with_stdout_data("2\n")
        .with_stderr_data(str![[r#"
[COMPILING] bar v0.5.0 ([ROOT]/wt/bar)
[COMPILING] foo v0.1.0 ([ROOT]/wt)
[FINISHED] `dev` profile [unoptimized + debuginfo] target(s) in [ELAPSED]s
[RUNNING] `target/debug/foo[EXE]`

"#]])
        .run();
    fork_cargo(&p, "run")
        .with_stdout_data("1\n")
        .with_stderr_data(str![[r#"
[FINISHED] `dev` profile [unoptimized + debuginfo] target(s) in [ELAPSED]s
[RUNNING] `target/debug/foo[EXE]`

"#]])
        .run();

    // Going back to earlier files reuses their units.
    fs::write(wt.join("bar/src/lib.rs"), "pub fn bar() -> u32 { 1 }").unwrap();
    fork_cargo(&p, "run")
        .cwd(&wt)
        .with_stdout_data("1\n")
        .with_stderr_data(str![[r#"
[FINISHED] `dev` profile [unoptimized + debuginfo] target(s) in [ELAPSED]s
[RUNNING] `target/debug/foo[EXE]`

"#]])
        .run();

    // Editing a test only rebuilds that test.
    fs::write(wt.join("bar/tests/t.rs"), "#[test] fn t2() {}").unwrap();
    fork_cargo(&p, "build")
        .cwd(&wt)
        .with_stderr_data(str![[r#"
[FINISHED] `dev` profile [unoptimized + debuginfo] target(s) in [ELAPSED]s

"#]])
        .run();

    // Opting out keeps the checkout in the hash.
    fork_cargo(&p, "build")
        .cwd(&wt)
        .env("CARGO_SHARE_WORKSPACE_UNITS", "0")
        .with_stderr_data(str![[r#"
[COMPILING] bar v0.5.0 ([ROOT]/wt/bar)
[COMPILING] foo v0.1.0 ([ROOT]/wt)
[FINISHED] `dev` profile [unoptimized + debuginfo] target(s) in [ELAPSED]s

"#]])
        .run();
}

#[cargo_test]
fn worktrees_share_incremental_caches() {
    let (p, wt) = repo_with_worktree("fn main() { bar::bar(); }");
    fork_cargo(&p, "build").env("CARGO_INCREMENTAL", "1").run();
    fs::write(wt.join("bar/src/lib.rs"), "pub fn bar() -> u32 { 2 }").unwrap();
    let mut build = fork_cargo(&p, "build");
    build
        .cwd(&wt)
        .env("CARGO_INCREMENTAL", "1")
        .arg("-v")
        .with_stderr_contains("[..] -Zremap-cwd-prefix=.[..]");
    if cfg!(unix) {
        // rustc writes through a link that stays the same across edits.
        build.with_stderr_contains(
            "[..]--out-dir [ROOT]/home/.cargo/targets/foo-[..]/debug/.out/bar-[..]",
        );
    }
    if !cargo_test_support::is_nightly() {
        build.with_stderr_contains("[..] -Zallow-features= [..]");
    }
    build.run();

    let bar_dirs = fs::read_dir(shared_build_dir().join("debug/incremental"))
        .unwrap()
        .filter(|e| {
            let name = e.as_ref().unwrap().file_name();
            name.to_str().unwrap().starts_with("bar-")
        })
        .count();
    assert_eq!(bar_dirs, 1);
}

#[cargo_test]
fn stable_crates_cannot_use_features() {
    if cargo_test_support::is_nightly() {
        return;
    }
    let (p, _wt) = repo_with_worktree("#![feature(rustc_attrs)] fn main() {}");
    fork_cargo(&p, "build")
        .with_status(101)
        .with_stderr_contains(
            "[..]the feature `rustc_attrs` is not in the list of allowed features",
        )
        .run();
}

#[cargo_test]
fn crates_reading_their_path_are_not_shared() {
    let (p, wt) = repo_with_worktree(
        "fn main() { bar::bar(); println!(\"{}\", env!(\"CARGO_MANIFEST_DIR\")); }",
    );
    fork_cargo(&p, "run")
        .with_stdout_data(str![[r#"
[ROOT]/foo

"#]])
        .run();
    fork_cargo(&p, "run")
        .cwd(&wt)
        .with_stdout_data(str![[r#"
[ROOT]/wt

"#]])
        .with_stderr_data(str![[r#"
[COMPILING] foo v0.1.0 ([ROOT]/wt)
[FINISHED] `dev` profile [unoptimized + debuginfo] target(s) in [ELAPSED]s
[RUNNING] `target/debug/foo[EXE]`

"#]])
        .run();
}

#[cargo_test]
fn version_names_the_turtton_release() {
    // TURTTON_VERSION is the single source of the release; read it at run time
    // so that the test does not depend on the manifest directory at build time.
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR").unwrap();
    let release = fs::read_to_string(PathBuf::from(manifest).join("TURTTON_VERSION")).unwrap();
    let release = release.trim();
    assert!(release.starts_with("0.102.0-") && release.contains("turtton"));

    let version = cargo::version().to_string();
    assert!(
        version.contains(&format!(
            "(turtton/cargo {release}, https://github.com/turtton/cargo)"
        )),
        "{version}"
    );
    assert!(!version.contains("huacnlee"), "{version}");
}

/// The fork no longer updates itself from huacnlee's releases. Run a copy of
/// the binary installed as `$CARGO_HOME/bin/cargo`, the place the old updater
/// recognised, with every switch that used to start it, and check that it
/// behaves like plain cargo: no stamp or log file, no connection to the
/// proxy that stands in for GitHub, and the internal child variable runs
/// the command instead of an update.
#[cargo_test]
fn self_update_cannot_be_started() {
    use std::net::TcpListener;

    let home = paths::cargo_home();
    let bin = home.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let installed = bin.join(format!("cargo{}", std::env::consts::EXE_SUFFIX));
    fs::copy(crate::utils::cargo_exe(), &installed).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());

    let expected = format!("cargo {}\n", cargo::version());
    for child_env in [None, Some(home.as_path())] {
        let mut p = cargo_test_support::process(&installed);
        p.arg("-V")
            .env_remove("CI")
            .env_remove("GITHUB_ACTIONS")
            .env_remove("__CARGO_FORK_DEFAULTS_OPT_OUT")
            .env("CARGO_HOME", &home)
            .env("CARGO_FORK_AUTO_UPDATE", "1")
            .env("https_proxy", &proxy)
            .env("HTTPS_PROXY", &proxy)
            .env("http_proxy", &proxy)
            .env("HTTP_PROXY", &proxy)
            .env("ALL_PROXY", &proxy);
        if let Some(child_env) = child_env {
            p.env("__CARGO_FORK_SELF_UPDATE", child_env);
        }
        cargo_test_support::execs()
            .with_process_builder(p)
            .with_stdout_data(expected.as_str())
            .run();
    }

    // The old code stamped before it spawned and logged before it exited, so
    // both files exist at once if either path is reachable.
    for name in [".fork-update", ".fork-update.log"] {
        assert!(!home.join(name).exists(), "{name} was written");
    }
    // Leave time for a detached child to connect, as the old one would.
    std::thread::sleep(std::time::Duration::from_secs(2));
    match listener.accept() {
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
        other => panic!("cargo contacted the proxy: {other:?}"),
    }
}
