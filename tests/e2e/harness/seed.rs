//! Seeding of the demo repositories: create the org + repos through the REST
//! API, then `git push` the fixture contents over HTTP with the system git.
//!
//! This is the single seeding implementation; both the Rust E2E tests and the
//! manual `tests/e2e/up.sh` path (the `e2e_stack` example) call [`seed_demo_repos`].

use std::fs;
use std::path::Path;

use eyre::{Context, Result};

use super::api::Api;
use super::fixtures::{self, OwnerKind, RepoFixture};
use super::git::Git;

/// Organization that owns the Actions demo repositories.
pub const DEMO_ORG: &str = "fjex-demo";

#[derive(Clone, Debug)]
pub struct SeededRepo {
    /// Fixture name, e.g. `ci-artifacts`.
    pub name: String,
    /// `owner/name` on the server.
    pub full_name: String,
    pub clone_url: String,
    pub workflows: Vec<String>,
    /// Extra branches pushed besides `main`.
    pub branches: Vec<String>,
}

pub struct SeedInput<'a> {
    pub api: &'a Api,
    pub base_url: &'a str,
    pub admin_username: &'a str,
    pub admin_token: &'a str,
    pub workflow_dir: &'a str,
    /// Short per-stack tag substituted for `__FJEX_STACK__` in fixtures.
    pub stack_tag: &'a str,
    /// Scratch directory for working trees.
    pub work_dir: &'a Path,
}

pub async fn seed_demo_repos(input: &SeedInput<'_>) -> Result<Vec<SeededRepo>> {
    input.api.create_org(input.admin_token, DEMO_ORG).await?;
    let git = Git::with_basic_auth(input.base_url, input.admin_username, input.admin_token);

    let mut seeded = Vec::new();
    for fixture in fixtures::load_all()? {
        let repo = seed_one(input, &git, &fixture)
            .await
            .wrap_err_with(|| format!("failed to seed demo repository {}", fixture.name))?;
        seeded.push(repo);
    }
    Ok(seeded)
}

async fn seed_one(input: &SeedInput<'_>, git: &Git, fixture: &RepoFixture) -> Result<SeededRepo> {
    let (org, owner) = match fixture.owner {
        OwnerKind::Org => (Some(DEMO_ORG), DEMO_ORG),
        OwnerKind::User => (None, input.admin_username),
    };
    input
        .api
        .create_repo(input.admin_token, org, &fixture.name, &fixture.description)
        .await?;
    let full_name = format!("{owner}/{}", fixture.name);

    let tree = input.work_dir.join(&fixture.name);
    if tree.exists() {
        fs::remove_dir_all(&tree)?;
    }
    fs::create_dir_all(&tree)?;
    let template = fixtures::Template {
        workflow_dir: input.workflow_dir,
        stack_tag: input.stack_tag,
    };
    fixtures::materialize(&fixture.dir.join("main"), &tree, template)?;

    git.run(&tree, &["init", "-q", "-b", "main"])?;
    git.run(&tree, &["add", "-A"])?;
    git.run(
        &tree,
        &[
            "commit",
            "-q",
            "-m",
            &format!("chore: seed {} demo repository", fixture.name),
        ],
    )?;
    for branch in &fixture.branches {
        git.run(&tree, &["checkout", "-q", "-b", &branch.name])?;
        fixtures::materialize(&branch.overlay, &tree, template)?;
        git.run(&tree, &["add", "-A"])?;
        git.run(&tree, &["commit", "-q", "-m", &branch.message])?;
        git.run(&tree, &["checkout", "-q", "main"])?;
    }

    // Push the extra branches first and main last: main is what triggers the
    // push workflows, and the default branch must exist before PRs target it.
    let url = git.repo_url(&full_name);
    let mut refspecs: Vec<String> = fixture
        .branches
        .iter()
        .map(|b| format!("refs/heads/{0}:refs/heads/{0}", b.name))
        .collect();
    refspecs.push("refs/heads/main:refs/heads/main".to_string());
    let mut args = vec!["push", "-q", url.as_str()];
    args.extend(refspecs.iter().map(String::as_str));
    git.run(&tree, &args)?;

    Ok(SeededRepo {
        name: fixture.name.clone(),
        full_name,
        clone_url: url,
        workflows: fixtures::workflow_files(fixture),
        branches: fixture.branches.iter().map(|b| b.name.clone()).collect(),
    })
}
