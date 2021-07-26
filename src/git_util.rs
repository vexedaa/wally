//! Contains helpers for wrangling git2.
//!
//! A good source of ideas for how we should be interacting with Git is
//! contained in Cargo's `git/utils.rs`:
//! https://github.com/rust-lang/cargo/blob/master/src/cargo/sources/git/utils.rs

use std::io;
use std::path::Path;

use anyhow::{bail, format_err, Context};
use git2::build::RepoBuilder;
use git2::{Cred, CredentialType, FetchOptions, RemoteCallbacks, Repository};
use url::Url;

/// Based roughly on Cargo's approach to handling authentication, but very pared
/// down.
///
/// https://github.com/rust-lang/cargo/blob/79b397d72c557eb6444a2ba0dc00a211a226a35a/src/cargo/sources/git/utils.rs#L588
fn make_credentials_callback(
    access_token: Option<String>,
    config: &git2::Config,
) -> impl (FnMut(&str, Option<&str>, CredentialType) -> Result<Cred, git2::Error>) + '_ {
    let mut cred_helper_tried = false;
    let mut token_tried = false;

    move |url, username, allowed_types| {
        if allowed_types.contains(CredentialType::USER_PASS_PLAINTEXT) {
            if let Some(token) = &access_token {
                if !token_tried {
                    token_tried = true;
                    return Cred::userpass_plaintext(&token, "");
                }
            } else {
                if !cred_helper_tried {
                    cred_helper_tried = true;
                    return Cred::credential_helper(config, url, username);
                }
            }
        }

        if allowed_types.contains(CredentialType::DEFAULT) {
            return Cred::default();
        }

        Err(git2::Error::from_str("no authentication available"))
    }
}

pub fn open_or_clone(
    access_token: Option<String>,
    url: &Url,
    path: &Path,
) -> anyhow::Result<Repository> {
    let repo = match Repository::open(path) {
        Ok(repo) => repo,
        Err(_) => {
            if let Err(err) = fs_err::remove_dir_all(path) {
                // If we were unable to remove the directory because it
                // wasn't found, that's fine! We didn't want it here
                // anyways.
                if err.kind() != io::ErrorKind::NotFound {
                    return Err(err.into());
                }
            }

            fs_err::create_dir_all(path)?;
            clone(access_token, url, path)
                .with_context(|| format!("Error cloning Git repository {}", url))?
        }
    };

    Ok(repo)
}

pub fn clone(access_token: Option<String>, url: &Url, into: &Path) -> anyhow::Result<Repository> {
    let git_config = git2::Config::open_default()?;

    let mut callbacks = RemoteCallbacks::new();
    callbacks.credentials(make_credentials_callback(access_token, &git_config));

    let mut fetch_options = FetchOptions::new();
    fetch_options.remote_callbacks(callbacks);

    let mut builder = RepoBuilder::new();
    builder.fetch_options(fetch_options);

    let repo = builder.clone(url.as_str(), into)?;
    Ok(repo)
}

pub fn commit_and_push(
    repository: &Repository,
    access_token: Option<String>,
    message: &str,
    modified_file: &Path,
) -> anyhow::Result<()> {
    let git_config = git2::Config::open_default()?;

    // git add $file
    let mut index = repository.index()?;
    index.add_path(modified_file)?;
    index.write()?;
    let tree_id = index.write_tree()?;
    let tree = repository.find_tree(tree_id)?;

    // git commit -m "..."
    let head = repository.head()?;
    let parent = repository.find_commit(head.target().unwrap())?;
    let sig = git2::Signature::now("PackageUser", "PackageUser@localhost")?;
    repository.commit(Some("HEAD"), &sig, &sig, &message, &tree, &[&parent])?;

    // git push
    let mut ref_status = Ok(());
    let mut callback_called = false;
    {
        let mut origin = repository.find_remote("origin")?;
        let mut callbacks = RemoteCallbacks::new();
        callbacks.credentials(make_credentials_callback(access_token, &git_config));
        callbacks.push_update_reference(|refname, status| {
            assert_eq!(refname, "refs/heads/main");
            if let Some(s) = status {
                ref_status = Err(format_err!("failed to push a ref: {}", s))
            }
            callback_called = true;
            Ok(())
        });
        let mut opts = git2::PushOptions::new();
        opts.remote_callbacks(callbacks);
        origin.push(&["refs/heads/main"], Some(&mut opts))?;
    }

    if !callback_called {
        bail!("update_reference callback was not called");
    }

    ref_status
}

pub fn update_index(access_token: Option<String>, repository: &Repository) -> anyhow::Result<()> {
    let git_config = git2::Config::open_default()?;

    let mut callbacks = RemoteCallbacks::new();
    callbacks.credentials(make_credentials_callback(access_token, &git_config));

    let mut fetch_options = FetchOptions::new();
    fetch_options.remote_callbacks(callbacks);

    repository
        .find_remote("origin")?
        .fetch(&["main"], Some(&mut fetch_options), None)
        .with_context(|| format!("could not fetch Git repository"))?;

    let mut options = git2::build::CheckoutBuilder::new();
    options.force();

    // "git reset --hard" to the latest commit in the remote repo
    let commit = repository.find_reference("FETCH_HEAD")?.peel_to_commit()?;
    repository
        .reset(
            &commit.into_object(),
            git2::ResetType::Hard,
            Some(&mut options),
        )
        .with_context(|| format!("could not reset git repo to fetch_head"))?;

    Ok(())
}
