use std::{
    collections::BTreeSet,
    env,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

fn main() -> ExitCode {
    let arguments: Vec<String> = env::args().skip(1).collect();
    if arguments.len() != 2 {
        eprintln!(
            "usage: collect-release-audit-evidence <previous-tag-or-commit> <target-revision>"
        );
        return ExitCode::from(2);
    }

    match collect_evidence(&arguments[0], &arguments[1]) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn collect_evidence(previous_revision: &str, target_revision: &str) -> Result<(), String> {
    let repo_root = PathBuf::from(run_jj(None, &["root"])?.trim());
    let previous_commit = resolve_commit(&repo_root, previous_revision)?;
    let target_commit = resolve_commit(&repo_root, target_revision)?;
    let target_version = version_at(&repo_root, &target_commit)?;
    let release_document = format!("docs/releases/{target_version}.md");
    let previous_tests = collect_tests(&repo_root, &previous_commit)?;
    let target_tests = collect_tests(&repo_root, &target_commit)?;

    println!("Previous commit: {previous_commit}");
    println!("Target commit:   {target_commit}");
    println!("Target version:  {target_version}");
    println!("Release document: {release_document}\n");

    println!("Changed files");
    print_command_output(run_jj(
        Some(&repo_root),
        &[
            "diff",
            "--from",
            &previous_commit,
            "--to",
            &target_commit,
            "--stat",
        ],
    )?);

    println!("Document changes");
    print_command_output(run_jj(
        Some(&repo_root),
        &[
            "diff",
            "--from",
            &previous_commit,
            "--to",
            &target_commit,
            "--",
            "DESIGN.md",
            &release_document,
        ],
    )?);

    println!("Tests added");
    for test in target_tests.difference(&previous_tests) {
        println!("{test}");
    }
    println!();

    println!("Tests removed");
    for test in previous_tests.difference(&target_tests) {
        println!("{test}");
    }
    println!();

    println!("Validation commands");
    println!("cargo fmt --check");
    println!("cargo test --all-targets");
    println!("cargo clippy --all-targets --all-features -- -D warnings");
    Ok(())
}

fn resolve_commit(repo_root: &Path, revision: &str) -> Result<String, String> {
    let resolved = run_jj(
        Some(repo_root),
        &[
            "log",
            "-r",
            revision,
            "--no-graph",
            "-T",
            "commit_id ++ \"\\n\"",
        ],
    )?;
    let commit = resolved.trim();
    if commit.len() != 40 || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!(
            "revision must resolve to exactly one commit: {revision}"
        ));
    }
    Ok(commit.to_owned())
}

fn version_at(repo_root: &Path, revision: &str) -> Result<String, String> {
    let manifest = run_jj(
        Some(repo_root),
        &["file", "show", "-r", revision, "Cargo.toml"],
    )?;
    manifest
        .lines()
        .find_map(|line| {
            line.strip_prefix("version = \"")
                .and_then(|value| value.split_once('"'))
                .map(|(version, _)| version.to_owned())
        })
        .ok_or_else(|| format!("Cargo.toml at {revision} has no package version"))
}

fn collect_tests(repo_root: &Path, revision: &str) -> Result<BTreeSet<String>, String> {
    let files = run_jj(Some(repo_root), &["file", "list", "-r", revision])?;
    let mut tests = BTreeSet::new();
    for source_file in files.lines().filter(|path| is_test_source(path)) {
        let source = run_jj(
            Some(repo_root),
            &["file", "show", "-r", revision, source_file],
        )?;
        for test_name in extract_test_names(&source) {
            tests.insert(format!("{source_file}::{test_name}"));
        }
    }
    Ok(tests)
}

fn is_test_source(path: &str) -> bool {
    path.ends_with(".rs")
        && ["src/", "examples/", "tests/", "benches/"]
            .iter()
            .any(|prefix| path.starts_with(prefix))
}

fn extract_test_names(source: &str) -> Vec<String> {
    let mut pending_test = false;
    let mut names = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("#[test]") || trimmed.starts_with("#[tokio::test]") {
            pending_test = true;
            continue;
        }
        if !pending_test {
            continue;
        }
        let Some(function) = line.find("fn ") else {
            continue;
        };
        let rest = &line[function + 3..];
        let name_length = rest
            .bytes()
            .take_while(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
            .count();
        let name = &rest[..name_length];
        if !name.is_empty() && rest[name_length..].trim_start().starts_with('(') {
            names.push(name.to_owned());
            pending_test = false;
        }
    }
    names
}

fn run_jj(current_dir: Option<&Path>, arguments: &[&str]) -> Result<String, String> {
    let mut command = Command::new("jj");
    command.args(arguments);
    if let Some(current_dir) = current_dir {
        command.current_dir(current_dir);
    }
    let output = command
        .output()
        .map_err(|error| format!("failed to run jj: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "`jj {}` failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout).map_err(|error| format!("jj emitted invalid UTF-8: {error}"))
}

fn print_command_output(output: String) {
    print!("{output}");
    if !output.ends_with('\n') {
        println!();
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_test_sources_to_rust_targets() {
        assert!(is_test_source("src/main.rs"));
        assert!(is_test_source("tests/nested/flow.rs"));
        assert!(is_test_source("benches/score.rs"));
        assert!(!is_test_source("build.rs"));
        assert!(!is_test_source("src/data.json"));
        assert!(!is_test_source(".agents/skills/example.rs"));
    }

    #[test]
    fn extracts_standard_and_tokio_test_names() {
        let source = r#"
            #[test]
            fn standard_test() {}

            #[tokio::test]
            async fn async_test() {}

            fn helper() {}
        "#;
        assert_eq!(
            extract_test_names(source),
            ["standard_test".to_owned(), "async_test".to_owned()]
        );
    }
}
