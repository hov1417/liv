use eyre::{eyre, Context, Result};
use chrono::Local;
use futures_util::stream::TryStreamExt;
use gix::create::{Kind, Options};
use octocrab::Octocrab;
use serde::Deserialize;
use std::env;
use std::fs;
use std::fs::File;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::AtomicBool;
use eyre::ContextCompat;
use tar::Builder;
use tokio::pin;
use tokio::{io::AsyncReadExt, process::Command};

struct ConfigVals {
    github_token: String,
    github_user: String,
}

#[derive(Debug, Deserialize)]
struct Repo {
    name: String,
    ssh_url: String,
    // clone_url: String, // if you prefer HTTPS, switch to this plus token handling
}

async fn read_file_if_exists(p: &Path) -> Result<String> {
    match tokio::fs::File::open(p).await {
        Ok(mut f) => {
            let mut s = String::new();
            f.read_to_string(&mut s).await?;
            Ok(s)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("reading {:?}", p)),
    }
}

fn parse_config(raw: &str) -> Result<ConfigVals> {
    let parsed =
        toml::from_str::<toml::Value>(raw).with_context(|| format!("parsing {:?}", raw))?;
    let github_token = env::var("GITHUB_TOKEN").ok().or_else(|| {
        parsed
            .get("github")
            .and_then(|v| v.as_table())
            .and_then(|v| v.get("token"))
            .and_then(|v| v.as_str())
            .map(|v| v.to_string())
    });
    let github_user = env::var("GITHUB_USER").ok().or_else(|| {
        parsed
            .get("github")
            .and_then(|v| v.as_table())
            .and_then(|v| v.get("user"))
            .and_then(|v| v.as_str())
            .map(|v| v.to_string())
    });
    Ok(ConfigVals {
        github_token: github_token.context("parsing GitHub token")?,
        github_user: github_user.context("parsing GitHub user")?,
    })
}

async fn list_user_repos(github_client: &Octocrab, cfg: &ConfigVals) -> Result<Vec<Repo>> {
    let repositories = github_client
        .users(cfg.github_user.as_str())
        .repos()
        .send()
        .await?
        .into_stream(github_client);
        // .take(3); // TODO, just for tests
    pin!(repositories);
    let mut result = Vec::new();
    while let Some(repository) = repositories.try_next().await? {
        result.push(Repo {
            name: repository.name,
            ssh_url: repository.ssh_url.unwrap(), // TODO unwrap?
        });
    }
    Ok(result)
}

async fn run_cmd(mut cmd: Command, what: &str) -> Result<()> {
    let status = cmd
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .with_context(|| format!("spawning {}", what))?;
    if !status.success() {
        return Err(eyre!("{} failed with {}", what, status));
    }
    Ok(())
}

async fn clean_dir(p: &Path) -> Result<()> {
    if p.exists() {
        println!("Cleaning {}", p.display());
        tokio::fs::remove_dir_all(p).await.ok(); // best-effort
    }
    Ok(())
}

async fn clone_repo(repo_name: &str, ssh_url: &str, dest: &Path) -> Result<()> {
    let repo_name = repo_name.to_owned();
    let ssh = ssh_url.to_owned();
    let dest = dest.to_owned();
    println!("Cloning {} to {}", ssh, dest.display());
    let (_r, _out) = tokio::task::spawn_blocking(move || {
        let mut prepare_fetch = gix::clone::PrepareFetch::new(
            ssh,
            dest,
            Kind::Bare,
            Options {
                destination_must_be_empty: true,
                fs_capabilities: None,
            },
            gix::open::Options::default(),
        )
        .context("Prepare fetch error")?;
        prepare_fetch
            .fetch_only(
                prodash::progress::Log::new(format!("Cloning {repo_name}"), None),
                &AtomicBool::new(false),
            )
            .context("Fetch error")
    })
    .await
    .context("Tokio spawn")??;

    println!("Cloned {}", ssh_url);
    Ok(())
}

// TODO use some compression or bundling zip, tar, zstd ...
async fn bundle_repo(repo_dir: &Path, bundle_path: &Path) -> Result<()> {
    println!(
        "Bundling {} -> {}",
        repo_dir.display(),
        bundle_path.display()
    );
    run_cmd(
        {
            let mut c = Command::new("git");
            c.arg("-C")
                .arg(repo_dir)
                .arg("bundle")
                .arg("create")
                .arg(bundle_path)
                .arg("--all");
            c
        },
        "git bundle create",
    )
    .await?;
    println!("Bundled {}", repo_dir.display());
    Ok(())
}

fn tar_bundles(src_dir: &Path, out_tar: &Path) -> Result<()> {
    println!("Creating tar {}", out_tar.display());
    if let Some(parent) = out_tar.parent() {
        fs::create_dir_all(parent)?;
    }
    let tar_file = File::create(out_tar)?;
    let mut builder = Builder::new(tar_file);

    for entry in walkdir::WalkDir::new(src_dir).min_depth(1).max_depth(1) {
        let entry = entry?;
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) == Some("bundle") {
            let mut f = File::open(p)?;
            let name_in_tar = p
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| eyre!("bad filename"))?;
            builder.append_file(name_in_tar, &mut f)?;
        }
    }

    builder.finish()?;
    println!("Tar written: {}", out_tar.display());
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    // Paths
    let home = dirs::home_dir().ok_or_else(|| eyre!("cannot find home dir"))?;
    let config_path = home.join(".config/liv/liv.toml");
    let tmp_dir = PathBuf::from("/tmp/github");
    let backup_dir = home.join("Backup/github");
    let date = Local::now().date_naive().to_string();
    let tar_out = backup_dir.join(format!("{}-bundles.tar", date));

    // Read config
    let raw_cfg = read_file_if_exists(&config_path).await?;
    let cfg =
        parse_config(&raw_cfg).with_context(|| format!("reading {:?}", config_path.display()))?;

    // Start fresh temp dir
    clean_dir(&tmp_dir).await?;
    tokio::fs::create_dir_all(&tmp_dir).await?;

    let github_client = octocrab::OctocrabBuilder::new()
        .personal_token(cfg.github_token.as_str())
        .build()?;

    let repos = list_user_repos(&github_client, &cfg).await?;
    if repos.is_empty() {
        eprintln!("No repositories returned.");
    }

    // Process repos concurrently (limit to avoid too many ssh connections)
    let concurrency = 6;
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(concurrency));

    let mut tasks = Vec::with_capacity(repos.len());
    for r in repos {
        let sem = sem.clone();
        let tmp_dir = tmp_dir.clone();
        tasks.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();

            let repo_name = r.name;
            let ssh_url = r.ssh_url;

            let repo_dir = tmp_dir.join(&repo_name);
            let bundle_path = tmp_dir.join(format!("{}.bundle", repo_name));

            // TODO error handling
            // Clone -> bundle -> clean
            if let Err(e) = clone_repo(&repo_name, &ssh_url, &repo_dir).await {
                eprintln!("[{}] clone error: {e:#}", repo_name);
                return;
            }
            if let Err(e) = bundle_repo(&repo_dir, &bundle_path).await {
                eprintln!("[{}] bundle error: {e:#}", repo_name);
                // try to clean even on failure
            }
            if let Err(e) = tokio::fs::remove_dir_all(&repo_dir).await {
                eprintln!("[{}] cleanup error: {e:#}", repo_name);
            }
        }));
    }
    for t in tasks {
        let _ = t.await;
    }

    // Tar all *.bundle into ~/Backup/github/YYYY-MM-DD-bundles.tar
    tar_bundles(&tmp_dir, &tar_out)?;

    // Final cleanup
    clean_dir(&tmp_dir).await?;
    println!("Done.");
    Ok(())
}
