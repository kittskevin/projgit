//! Phase 0a spike — on-demand single-blob fetch via gitoxide.
//!
//! Goal: against a partial clone made with
//! `git clone --filter=blob:none --no-checkout <url>`, ask `gix` to fetch
//! one specific blob OID that is referenced by the cloned tree but not
//! present in the local object store.
//!
//! Outcome decides the design of `projgit-core`'s Fetcher (see
//! `docs/initial-plan.md` §5.4).
//!
//! Usage:
//!
//! ```text
//! # one-time setup of a small partial clone:
//! git clone --filter=blob:none --no-checkout \
//!     https://github.com/rust-lang/log spikes/ondemand-fetch/testrepo
//!
//! # then:
//! cd spikes/ondemand-fetch && cargo run --release
//! ```

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::time::Instant;

const DEFAULT_REPO: &str = "testrepo";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let repo_path = args
        .get(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_REPO));

    println!("== Phase 0a spike: gitoxide on-demand fetch ==");
    println!("Repo path: {}", repo_path.display());

    if !repo_path.join(".git").exists() && !repo_path.join("HEAD").exists() {
        bail!(
            "No git repo at {0}.\n\
             Set one up with:\n  \
             git clone --filter=blob:none --no-checkout \\\n    \
             https://github.com/rust-lang/log {0}",
            repo_path.display()
        );
    }

    // ---- 1. Open the repository -----------------------------------------
    let t = Instant::now();
    let repo = gix::open(&repo_path).context("gix::open")?;
    println!("[{:?}] Opened repo. git_dir = {}", t.elapsed(), repo.git_dir().display());

    // ---- 2. Resolve HEAD -> commit -> tree ------------------------------
    let mut head = repo.head().context("repo.head()")?;
    let head_commit = head.peel_to_commit_in_place().context("peel_to_commit")?;
    println!("HEAD commit: {}", head_commit.id());
    let head_tree = head_commit.tree().context("head_commit.tree()")?;
    println!("HEAD tree:   {}", head_tree.id());

    // ---- 3. Walk the tree and pick one blob -----------------------------
    let blob_oid = find_first_blob(&repo, head_tree.id().detach())
        .context("walking tree to find a blob")?;
    println!("Test blob OID: {}", blob_oid);

    // ---- 4. Is it locally present? --------------------------------------
    let present_before = repo
        .try_find_object(blob_oid)
        .context("try_find_object before fetch")?
        .is_some();
    println!("Locally present BEFORE fetch: {}", present_before);

    if present_before {
        println!(
            "NOTE: blob is already in the local store. \
             This clone may not be blob:none, or the blob is small enough \
             to have been included. Re-run after a fresh blobless clone, \
             or pick a deeper file in find_first_blob()."
        );
        return Ok(());
    }

    // ---- 5. Attempt on-demand fetch -------------------------------------
    println!("\n--- Attempting on-demand fetch via gix ---");
    let t = Instant::now();
    let fetch_result = try_ondemand_fetch(&repo, blob_oid);
    let fetch_elapsed = t.elapsed();
    match &fetch_result {
        Ok(()) => println!("[{:?}] gix on-demand fetch returned Ok.", fetch_elapsed),
        Err(e) => println!("[{:?}] gix on-demand fetch FAILED: {:#}", fetch_elapsed, e),
    }

    // ---- 6. Verify ------------------------------------------------------
    let after = repo
        .try_find_object(blob_oid)
        .context("try_find_object after fetch")?;
    let present_after = after.is_some();
    println!("Locally present AFTER fetch:  {}", present_after);

    if let Some(obj) = after {
        println!("Fetched blob size: {} bytes", obj.data.len());
        println!("\n*** SPIKE RESULT: Branch A (gix on-demand fetch WORKS) ***");
    } else {
        println!("\n*** SPIKE RESULT: Branch B (gix on-demand fetch DOES NOT WORK today) ***");
        println!("Fallback path: shell out to `git cat-file --batch` against the promisor remote.");
    }

    Ok(())
}

/// Walk the tree (one level for now) and return the first blob entry's OID.
fn find_first_blob(
    repo: &gix::Repository,
    tree_oid: gix::ObjectId,
) -> Result<gix::ObjectId> {
    let tree = repo.find_object(tree_oid)?.into_tree();
    for entry in tree.iter() {
        let entry = entry?;
        if entry.mode().is_blob() {
            return Ok(entry.oid().to_owned());
        }
    }
    bail!("no blob entries at the top level of the tree; recurse deeper");
}

/// Attempt to ask gix to fetch one specific blob OID from the promisor
/// remote. The exact API is what this spike is meant to discover; this is
/// our first guess.
fn try_ondemand_fetch(repo: &gix::Repository, oid: gix::ObjectId) -> Result<()> {
    use gix::remote::Direction;

    // Approach: configure `origin` with a single refspec that names the OID
    // as a want. Servers that support `allow-tip-sha1-in-want` /
    // `allow-reachable-sha1-in-want` (GitHub, GitLab, most modern hosts)
    // should accept it. Stock git does this for `git fetch origin <oid>`.
    let refspec = format!("+{}:refs/projgit-spike/wanted", oid);
    println!("  Using refspec: {}", refspec);

    let mut remote = repo
        .find_remote("origin")
        .context("find_remote(origin)")?;
    remote
        .replace_refspecs([refspec.as_str()], Direction::Fetch)
        .context("replace_refspecs")?;

    println!("  Connecting to remote...");
    let conn = remote
        .connect(Direction::Fetch)
        .context("remote.connect(Fetch)")?;

    println!("  Preparing fetch...");
    let prep = conn
        .prepare_fetch(&mut gix::progress::Discard, Default::default())
        .context("prepare_fetch")?;

    println!("  Receiving pack...");
    let _outcome = prep
        .receive(&mut gix::progress::Discard, &gix::interrupt::IS_INTERRUPTED)
        .context("receive")?;

    println!("  receive() returned Ok.");
    Ok(())
}
