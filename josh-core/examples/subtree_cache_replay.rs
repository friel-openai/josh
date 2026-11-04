//! Reproducible local experiment, not a production workload simulator.
//! Reads real trees through an alternate ODB; only the scratch repository is written.
//! Usage: subtree_cache_replay OBJECTS COMMITS PATHS MODE CAPACITY PATH_COUNT CHANGE_EVERY SCRATCH
//! COMMITS is newest-first SHA lines; PATHS is `git ls-tree -rz --name-only`.
//! MODE: keep/remove (tree replay), history-keep/history-remove/history-pin
//! History replay reparents snapshots into a short linear history, with frozen
//! selectors supplied through a hook. It does not replay production policy/merges.
use anyhow::{Context, Result, ensure};
use josh_core::{cache, filter};
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Instant};

struct Hook(HashMap<git2::Oid, filter::Filter>);
impl cache::FilterHook for Hook {
    fn filter_for_commit(&self, oid: git2::Oid, _: &str) -> Result<filter::Filter> {
        self.0.get(&oid).copied().context("missing frozen selector")
    }
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    ensure!(
        args.len() == 8,
        "expected OBJECTS COMMITS PATHS MODE CAPACITY PATH_COUNT CHANGE_EVERY SCRATCH"
    );
    let capacity = args[4].parse::<usize>()?;
    let count = args[5].parse::<usize>()?;
    let change_every = args[6].parse::<usize>()?;
    let scratch = PathBuf::from(&args[7]);
    ensure!(!scratch.exists(), "scratch must be new");
    let repo = git2::Repository::init_bare(&scratch)?;
    std::fs::write(
        repo.path().join("objects/info/alternates"),
        format!("{}\n", args[0]),
    )?;
    let source_commits = std::fs::read_to_string(&args[1])?
        .lines()
        .map(git2::Oid::from_str)
        .collect::<Result<Vec<_>, _>>()?;
    let trees = source_commits
        .iter()
        .rev()
        .map(|id| repo.find_commit(*id).map(|c| c.tree_id()))
        .collect::<Result<Vec<_>, _>>()?;
    ensure!(!trees.is_empty(), "empty commit list");
    let bytes = std::fs::read(&args[2])?;
    let all_paths: Vec<_> = bytes
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .filter_map(|p| std::str::from_utf8(p).ok())
        .collect();
    ensure!(
        count > 0 && all_paths.len() >= count + 1,
        "need more input paths than selected paths"
    );
    // Evenly spaced across the sorted inventory, rather than taking one directory.
    let paths: Vec<_> = (0..count)
        .map(|i| PathBuf::from(all_paths[i * all_paths.len() / count]))
        .collect();
    let selected: std::collections::HashSet<_> = paths.iter().collect();
    let extras: Vec<_> = all_paths
        .iter()
        .map(PathBuf::from)
        .filter(|p| !selected.contains(p))
        .take(trees.len())
        .collect();
    let mut policies = HashMap::new();
    let mut selectors = Vec::new();
    let mut commits = Vec::new();
    let mut hook = HashMap::new();
    let signature = git2::Signature::new(
        "Subtree replay",
        "replay@example.com",
        &git2::Time::new(1_700_000_000, 0),
    )?;
    for (i, tree) in trees.iter().enumerate() {
        let policy_index = if change_every == 0 {
            0
        } else {
            i / change_every
        };
        let selector = *policies.entry(policy_index).or_insert_with(|| {
            let mut paths = paths.clone();
            if policy_index != 0 {
                paths.pop();
                paths.push(extras[policy_index % extras.len()].clone());
            }
            filter::files(paths)
        });
        selectors.push(selector);
        if args[3].starts_with("history-") {
            let parents = commits.last().map(|id| repo.find_commit(*id)).transpose()?;
            let parents: Vec<_> = parents.iter().collect();
            let oid = repo.commit(
                None,
                &signature,
                &signature,
                &format!("snapshot {i}"),
                &repo.find_tree(*tree)?,
                &parents,
            )?;
            let f = match args[3].as_str() {
                "history-keep" => selector,
                "history-remove" => filter::Filter::new().exclude(selector),
                "history-pin" if i == 0 => filter::Filter::default(),
                "history-pin" => filter::Filter::new().pin(selector),
                _ => anyhow::bail!("unknown mode"),
            };
            commits.push(oid);
            hook.insert(oid, f);
        }
    }
    cache::sled_load(&scratch)?;
    let transaction = cache::TransactionContext::new(&scratch, cache::CacheStack::new().into())
        .with_identity_selection_cache_capacity(capacity)
        .ephemeral()
        .open()?
        .with_filter_hook(Arc::new(Hook(hook)));
    let mut outputs = Vec::new();
    let mut snapshot_seconds = Vec::new();
    let history_filter = filter::parse(":hook=replay")?.with_meta("history", "no-splice");
    let start = Instant::now();
    if args[3].starts_with("history-") {
        for commit in &commits {
            let snapshot_start = Instant::now();
            let oid = josh_core::filter_commit(&transaction, history_filter, *commit)?;
            outputs.push((
                oid.to_string(),
                if oid.is_zero() {
                    String::new()
                } else {
                    transaction.repo().find_commit(oid)?.tree_id().to_string()
                },
            ));
            snapshot_seconds.push(snapshot_start.elapsed().as_secs_f64());
        }
    } else {
        for (tree, selector) in trees.iter().zip(&selectors) {
            let snapshot_start = Instant::now();
            let f = match args[3].as_str() {
                "keep" => *selector,
                "remove" => filter::Filter::new().exclude(*selector),
                _ => anyhow::bail!("unknown mode"),
            };
            let output = filter::apply(
                &transaction,
                f,
                filter::Rewrite::from_tree(transaction.repo().find_tree(*tree)?),
            )?;
            outputs.push((String::new(), output.tree().id().to_string()));
            snapshot_seconds.push(snapshot_start.elapsed().as_secs_f64());
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    let stats = transaction.identity_selection_cache_stats();
    // A repeat in the same transaction checks warmed cache correctness separately.
    let warm_start = Instant::now();
    if let Some(commit) = commits.last() {
        let oid = josh_core::filter_commit(&transaction, history_filter, *commit)?;
        ensure!(
            outputs.last().unwrap().0 == oid.to_string(),
            "warm history changed"
        );
    }
    let warm_seconds = warm_start.elapsed().as_secs_f64();
    let rss = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM:"))
                .map(str::to_owned)
        });
    println!(
        "{}",
        serde_json::json!({
            "mode":args[3], "capacity":capacity, "input_paths":all_paths.len(),
            "selected_paths":count, "snapshots":trees.len(), "change_every":change_every,
            "policies":policies.len(), "seconds":elapsed, "warm_seconds":warm_seconds,
            "hits":stats.hits, "misses":stats.misses, "evictions":stats.evictions,
            "entries":stats.entries, "peak_rss":rss, "outputs":outputs,
            "snapshot_seconds":snapshot_seconds,
            "source_commits":source_commits.iter().map(ToString::to_string).collect::<Vec<_>>(),
        })
    );
    Ok(())
}
