//! Demo repository fixtures under `tests/e2e/fixtures/repos/<name>/`.
//!
//! Layout of one fixture:
//!
//! ```text
//! <name>/repo.json            owner ("org" | "user"), description, extra branches
//! <name>/main/...             files committed on `main`
//! <name>/branches/<dir>/...   overlay committed on top of main for an extra branch
//! ```
//!
//! Templating (see [`Template`]):
//!
//! - path segments named `__WORKFLOW_DIR__` become the target's workflow
//!   directory (`.forgejo` or `.gitea`);
//! - `__FJEX_STACK__` inside file contents becomes a short per-stack tag.
//!   Workflows put it in their `name:`. Runners derive job container names
//!   from the task id (which starts at 1 on every server) and the workflow
//!   name, so the tag keeps parallel stacks on one Docker daemon from
//!   colliding.

use std::fs;
use std::path::{Path, PathBuf};

use eyre::{eyre, Context, Result};
use serde_json::Value;

pub const WORKFLOW_DIR_PLACEHOLDER: &str = "__WORKFLOW_DIR__";
pub const STACK_PLACEHOLDER: &str = "__FJEX_STACK__";

/// Values substituted while materializing a fixture.
#[derive(Clone, Copy, Debug)]
pub struct Template<'a> {
    pub workflow_dir: &'a str,
    pub stack_tag: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnerKind {
    /// Owned by the seeded organization.
    Org,
    /// Owned by the admin user.
    User,
}

#[derive(Clone, Debug)]
pub struct BranchFixture {
    pub name: String,
    pub overlay: PathBuf,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct RepoFixture {
    pub name: String,
    pub owner: OwnerKind,
    pub description: String,
    pub dir: PathBuf,
    pub branches: Vec<BranchFixture>,
}

pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/e2e/fixtures/repos")
}

/// All fixtures, sorted by name.
pub fn load_all() -> Result<Vec<RepoFixture>> {
    let root = root();
    let mut out = Vec::new();
    for entry in
        fs::read_dir(&root).wrap_err_with(|| format!("failed to read {}", root.display()))?
    {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            out.push(load(&entry.path())?);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

pub fn load(dir: &Path) -> Result<RepoFixture> {
    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| eyre!("invalid fixture dir {}", dir.display()))?
        .to_string();
    let manifest_path = dir.join("repo.json");
    let manifest: Value = serde_json::from_str(
        &fs::read_to_string(&manifest_path)
            .wrap_err_with(|| format!("failed to read {}", manifest_path.display()))?,
    )
    .wrap_err_with(|| format!("invalid JSON in {}", manifest_path.display()))?;

    let owner = match manifest["owner"].as_str() {
        Some("org") => OwnerKind::Org,
        Some("user") => OwnerKind::User,
        other => {
            return Err(eyre!(
                "{}: owner must be \"org\" or \"user\", got {other:?}",
                manifest_path.display()
            ))
        }
    };
    let description = manifest["description"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let mut branches = Vec::new();
    for b in manifest["branches"].as_array().into_iter().flatten() {
        let name = b["name"]
            .as_str()
            .ok_or_else(|| eyre!("{}: branch without name", manifest_path.display()))?;
        let overlay = b["overlay"]
            .as_str()
            .ok_or_else(|| eyre!("{}: branch {name} without overlay", manifest_path.display()))?;
        branches.push(BranchFixture {
            name: name.to_string(),
            overlay: dir.join(overlay),
            message: b["message"].as_str().unwrap_or("update").to_string(),
        });
    }
    if !dir.join("main").is_dir() {
        return Err(eyre!("fixture {name} has no main/ directory"));
    }
    for b in &branches {
        if !b.overlay.is_dir() {
            return Err(eyre!(
                "fixture {name}: overlay {} does not exist",
                b.overlay.display()
            ));
        }
    }
    Ok(RepoFixture {
        name,
        owner,
        description,
        dir: dir.to_path_buf(),
        branches,
    })
}

/// Copy `src` into `dest`, applying the [`Template`].
pub fn materialize(src: &Path, dest: &Path, template: Template<'_>) -> Result<usize> {
    let mut copied = 0;
    for entry in fs::read_dir(src).wrap_err_with(|| format!("failed to read {}", src.display()))? {
        let entry = entry?;
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        let target_name = if file_name == WORKFLOW_DIR_PLACEHOLDER {
            template.workflow_dir.to_string()
        } else {
            file_name.to_string()
        };
        let target = dest.join(target_name);
        if entry.file_type()?.is_dir() {
            fs::create_dir_all(&target)?;
            copied += materialize(&entry.path(), &target, template)?;
        } else {
            let bytes = fs::read(entry.path())
                .wrap_err_with(|| format!("failed to read {}", entry.path().display()))?;
            let bytes = match String::from_utf8(bytes) {
                Ok(text) => text
                    .replace(STACK_PLACEHOLDER, template.stack_tag)
                    .into_bytes(),
                Err(e) => e.into_bytes(),
            };
            fs::write(&target, bytes)
                .wrap_err_with(|| format!("failed to write {}", target.display()))?;
            copied += 1;
        }
    }
    Ok(copied)
}

/// Workflow file names (e.g. `ci.yml`) shipped by a fixture on main.
pub fn workflow_files(fixture: &RepoFixture) -> Vec<String> {
    let dir = fixture
        .dir
        .join("main")
        .join(WORKFLOW_DIR_PLACEHOLDER)
        .join("workflows");
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|it| {
            it.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_fixtures_load_and_materialize_for_both_workflow_dirs() {
        let fixtures = load_all().expect("fixtures load");
        let names: Vec<&str> = fixtures.iter().map(|f| f.name.as_str()).collect();
        for expected in [
            "ci-artifacts",
            "ci-mixed",
            "dispatch-inputs",
            "pulls-demo",
            "slow-job",
        ] {
            assert!(
                names.contains(&expected),
                "missing fixture {expected}: {names:?}"
            );
        }
        for wf_dir in [".forgejo", ".gitea"] {
            for f in &fixtures {
                let tmp = tempfile::tempdir().unwrap();
                let template = Template {
                    workflow_dir: wf_dir,
                    stack_tag: "test1234",
                };
                let n = materialize(&f.dir.join("main"), tmp.path(), template).unwrap();
                assert!(n > 0, "{} copied no files", f.name);
                let wf = tmp.path().join(wf_dir).join("workflows");
                assert_eq!(
                    wf.is_dir(),
                    !workflow_files(f).is_empty(),
                    "{}: workflow dir mismatch",
                    f.name
                );
                assert!(!tmp.path().join(WORKFLOW_DIR_PLACEHOLDER).exists());
                for name in workflow_files(f) {
                    let text = fs::read_to_string(wf.join(&name)).unwrap();
                    assert!(
                        text.lines()
                            .next()
                            .is_some_and(|l| l.starts_with("name: ") && l.ends_with("[test1234]")),
                        "{}/{name}: first line must be `name: <x> [{STACK_PLACEHOLDER}]`",
                        f.name
                    );
                }
            }
        }
    }

    /// `run: echo "a: b"` is invalid YAML (a plain scalar may not contain ": ");
    /// servers reject such workflows at run time, so catch it here.
    #[test]
    fn workflow_run_steps_use_block_scalars_when_they_contain_colons() {
        for f in load_all().unwrap() {
            for wf in workflow_files(&f) {
                let path = f
                    .dir
                    .join("main")
                    .join(WORKFLOW_DIR_PLACEHOLDER)
                    .join("workflows")
                    .join(&wf);
                for (n, line) in fs::read_to_string(&path).unwrap().lines().enumerate() {
                    let t = line.trim_start().trim_start_matches("- ");
                    if let Some(v) = t.strip_prefix("run:") {
                        let v = v.trim();
                        assert!(
                            v.starts_with('|') || v.starts_with('>') || !v.contains(": "),
                            "{}:{}: use `run: |` for commands containing \": \"",
                            path.display(),
                            n + 1
                        );
                    }
                }
            }
        }
    }
}
