/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![deny(warnings)]

use anyhow::{anyhow, Error};
use blobrepo::BlobRepo;
use blobrepo_hg::BlobRepoHg;
use blobstore::Loadable;
use cloned::cloned;
use context::CoreContext;
use futures::{future::try_join_all, TryStreamExt};
use manifest::get_implicit_deletes;
use megarepo_configs::types::SourceMappingRules;
use mercurial_types::HgManifestId;
use mononoke_types::{BonsaiChangesetMut, ChangesetId, FileChange, MPath};
use sorted_vector_map::SortedVectorMap;
use std::{collections::HashMap, sync::Arc};
use thiserror::Error;

pub type MultiMover = Arc<dyn Fn(&MPath) -> Result<Vec<MPath>, Error> + Send + Sync + 'static>;

#[derive(Debug, Error)]
pub enum ErrorKind {
    #[error("Remapped commit {0} expected in target repo, but not present")]
    MissingRemappedCommit(ChangesetId),
}

pub fn create_source_to_target_multi_mover(
    mapping_rules: SourceMappingRules,
) -> Result<MultiMover, Error> {
    // We apply the longest prefix first
    let mut overrides = mapping_rules.overrides.into_iter().collect::<Vec<_>>();
    overrides.sort_unstable_by_key(|(ref prefix, _)| prefix.len());
    overrides.reverse();
    let prefix = MPath::new_opt(mapping_rules.default_prefix)?;

    Ok(Arc::new(move |path: &MPath| -> Result<Vec<MPath>, Error> {
        for (override_prefix_src, dsts) in &overrides {
            let override_prefix_src = MPath::new(override_prefix_src.clone())?;
            if override_prefix_src.is_prefix_of(path) {
                let suffix: Vec<_> = path
                    .into_iter()
                    .skip(override_prefix_src.num_components())
                    .collect();

                return dsts
                    .iter()
                    .map(|dst| {
                        let override_prefix = MPath::new_opt(dst)?;
                        MPath::join_opt(override_prefix.as_ref(), suffix.clone())
                            .ok_or_else(|| anyhow!("unexpected empty path"))
                    })
                    .collect::<Result<_, _>>();
            }
        }

        Ok(vec![
            MPath::join_opt(prefix.as_ref(), path)
                .ok_or_else(|| anyhow!("unexpected empty path"))?,
        ])
    }))
}

/// Get `HgManifestId`s for a set of `ChangesetId`s
/// This is needed for the purposes of implicit delete detection
async fn get_manifest_ids<'a, I: IntoIterator<Item = ChangesetId>>(
    ctx: &'a CoreContext,
    repo: &'a BlobRepo,
    bcs_ids: I,
) -> Result<Vec<HgManifestId>, Error> {
    try_join_all(bcs_ids.into_iter().map({
        |bcs_id| {
            cloned!(ctx, repo);
            async move {
                let cs_id = repo
                    .get_hg_from_bonsai_changeset(ctx.clone(), bcs_id)
                    .await?;
                let hg_blob_changeset = cs_id.load(&ctx, repo.blobstore()).await?;
                Ok(hg_blob_changeset.manifestid())
            }
        }
    }))
    .await
}

/// Take an iterator of file changes, which may contain implicit deletes
/// and produce a `SortedVectorMap` suitable to be used in the `BonsaiChangeset`,
/// without any implicit deletes.
fn minimize_file_change_set<FC, I: IntoIterator<Item = (MPath, Option<FC>)>>(
    file_changes: I,
) -> SortedVectorMap<MPath, Option<FC>> {
    let (adds, removes): (Vec<_>, Vec<_>) =
        file_changes.into_iter().partition(|(_, fc)| fc.is_some());
    let adds: HashMap<MPath, Option<FC>> = adds.into_iter().collect();

    let prefix_path_was_added = |removed_path: MPath| {
        removed_path
            .into_parent_dir_iter()
            .any(|parent_dir| adds.contains_key(&parent_dir))
    };

    let filtered_removes = removes
        .into_iter()
        .filter(|(ref mpath, _)| !prefix_path_was_added(mpath.clone()));
    let mut result: SortedVectorMap<_, _> = filtered_removes.collect();
    result.extend(adds.into_iter());
    result
}

/// Given a changeset and it's parents, get the list of file
/// changes, which arise from "implicit deletes" as opposed
/// to naive `MPath` rewriting in `cs.file_changes`. For
/// more information about implicit deletes, please see
/// `manifest/src/implici_deletes.rs`
async fn get_implicit_delete_file_changes<'a, I: IntoIterator<Item = ChangesetId>>(
    ctx: &'a CoreContext,
    cs: BonsaiChangesetMut,
    parent_changeset_ids: I,
    mover: MultiMover,
    source_repo: &'a BlobRepo,
) -> Result<Vec<(MPath, Option<FileChange>)>, Error> {
    let parent_manifest_ids = get_manifest_ids(ctx, source_repo, parent_changeset_ids).await?;
    let file_adds: Vec<_> = cs
        .file_changes
        .iter()
        .filter_map(|(mpath, maybe_file_change)| maybe_file_change.as_ref().map(|_| mpath.clone()))
        .collect();
    let store = source_repo.get_blobstore();
    let implicit_deletes: Vec<MPath> =
        get_implicit_deletes(ctx, store, file_adds, parent_manifest_ids)
            .try_collect()
            .await?;
    let maybe_renamed_implicit_deletes: Result<Vec<Vec<MPath>>, _> =
        implicit_deletes.iter().map(|mpath| mover(mpath)).collect();
    let maybe_renamed_implicit_deletes: Vec<Vec<MPath>> = maybe_renamed_implicit_deletes?;
    let implicit_delete_file_changes: Vec<_> = maybe_renamed_implicit_deletes
        .into_iter()
        .flatten()
        .map(|implicit_delete_mpath| (implicit_delete_mpath, None))
        .collect();

    Ok(implicit_delete_file_changes)
}

/// Create a version of `cs` with `Mover` applied to all changes
/// The return value can be:
/// - `Err` if the rewrite failed
/// - `Ok(None)` if the rewrite decided that this commit should
///              not be present in the rewrite target
/// - `Ok(Some(rewritten))` for a successful rewrite, which should be
///                         present in the rewrite target
/// The notion that the commit "should not be present in the rewrite
/// target" means that the commit is not a merge and all of its changes
/// were rewritten into nothingness by the `Mover`.
///
/// Precondition: this function expects all `cs` parents to be present
/// in `remapped_parents` as keys, and their remapped versions as values.
pub async fn rewrite_commit<'a>(
    ctx: &'a CoreContext,
    mut cs: BonsaiChangesetMut,
    remapped_parents: &'a HashMap<ChangesetId, ChangesetId>,
    mover: MultiMover,
    source_repo: BlobRepo,
) -> Result<Option<BonsaiChangesetMut>, Error> {
    if !cs.file_changes.is_empty() {
        let implicit_delete_file_changes = get_implicit_delete_file_changes(
            ctx,
            cs.clone(),
            remapped_parents.keys().cloned(),
            mover.clone(),
            &source_repo,
        )
        .await?;

        let path_rewritten_changes: Result<Vec<Vec<_>>, _> = cs
            .file_changes
            .into_iter()
            .map(|(path, change)| {
                // Just rewrite copy_from information, when we have it
                fn rewrite_copy_from(
                    copy_from: &(MPath, ChangesetId),
                    remapped_parents: &HashMap<ChangesetId, ChangesetId>,
                    mover: MultiMover,
                ) -> Result<Option<(MPath, ChangesetId)>, Error> {
                    let (path, copy_from_commit) = copy_from;
                    let new_paths = mover(&path)?;
                    let copy_from_commit =
                        remapped_parents.get(copy_from_commit).ok_or_else(|| {
                            Error::from(ErrorKind::MissingRemappedCommit(*copy_from_commit))
                        })?;

                    // If the source path doesn't remap, drop this copy info.

                    // TODO(stash): a path can be remapped to multiple other paths,
                    // but for copy_from path we pick only the first one. Instead of
                    // picking only the first one, it's a better to have a dedicated
                    // field in a thrift struct which says which path should be picked
                    // as copy from
                    Ok(new_paths
                        .get(0)
                        .cloned()
                        .map(|new_path| (new_path, *copy_from_commit)))
                }

                // Extract any copy_from information, and use rewrite_copy_from on it
                fn rewrite_file_change(
                    change: FileChange,
                    remapped_parents: &HashMap<ChangesetId, ChangesetId>,
                    mover: MultiMover,
                ) -> Result<FileChange, Error> {
                    let new_copy_from = change
                        .copy_from()
                        .and_then(|copy_from| {
                            rewrite_copy_from(copy_from, remapped_parents, mover).transpose()
                        })
                        .transpose()?;

                    Ok(FileChange::with_new_copy_from(change, new_copy_from))
                }

                // Rewrite both path and changes
                fn do_rewrite(
                    path: MPath,
                    change: Option<FileChange>,
                    remapped_parents: &HashMap<ChangesetId, ChangesetId>,
                    mover: MultiMover,
                ) -> Result<Vec<(MPath, Option<FileChange>)>, Error> {
                    let new_paths = mover(&path)?;
                    let change = change
                        .map(|change| rewrite_file_change(change, remapped_parents, mover.clone()))
                        .transpose()?;
                    Ok(new_paths
                        .into_iter()
                        .map(|new_path| (new_path, change.clone()))
                        .collect())
                }
                do_rewrite(path, change, &remapped_parents, mover.clone())
            })
            .collect();

        let mut path_rewritten_changes: SortedVectorMap<_, _> = path_rewritten_changes?
            .into_iter()
            .map(|changes| changes.into_iter())
            .flatten()
            .collect();

        path_rewritten_changes.extend(implicit_delete_file_changes.into_iter());
        let path_rewritten_changes = minimize_file_change_set(path_rewritten_changes.into_iter());
        let is_merge = cs.parents.len() >= 2;

        // If all parent has < 2 commits then it's not a merge, and it was completely rewritten
        // out. In that case we can just discard it because there are not changes to the working copy.
        // However if it's a merge then we can't discard it, because even
        // though bonsai merge commit might not have file changes inside it can still change
        // a working copy. E.g. if p1 has fileA, p2 has fileB, then empty merge(p1, p2)
        // contains both fileA and fileB.
        if path_rewritten_changes.is_empty() && !is_merge {
            return Ok(None);
        } else {
            cs.file_changes = path_rewritten_changes;
        }
    }

    // Update hashes
    for commit in cs.parents.iter_mut() {
        let remapped = remapped_parents
            .get(commit)
            .ok_or_else(|| Error::from(ErrorKind::MissingRemappedCommit(*commit)))?;

        *commit = *remapped;
    }

    Ok(Some(cs))
}

#[cfg(test)]
mod test {
    use super::*;
    use blobrepo::save_bonsai_changesets;
    use fbinit::FacebookInit;
    use maplit::{btreemap, hashmap};
    use std::collections::BTreeMap;
    use test_repo_factory::TestRepoFactory;
    use tests_utils::{list_working_copy_utf8, CreateCommitContext};

    #[test]
    fn test_multi_mover_simple() -> Result<(), Error> {
        let mapping_rules = SourceMappingRules {
            default_prefix: "".to_string(),
            ..Default::default()
        };
        let multi_mover = create_source_to_target_multi_mover(mapping_rules)?;
        assert_eq!(
            multi_mover(&MPath::new("path")?)?,
            vec![MPath::new("path")?]
        );
        Ok(())
    }

    #[test]
    fn test_multi_mover_prefixed() -> Result<(), Error> {
        let mapping_rules = SourceMappingRules {
            default_prefix: "prefix".to_string(),
            ..Default::default()
        };
        let multi_mover = create_source_to_target_multi_mover(mapping_rules)?;
        assert_eq!(
            multi_mover(&MPath::new("path")?)?,
            vec![MPath::new("prefix/path")?]
        );
        Ok(())
    }

    #[test]
    fn test_multi_mover_prefixed_with_exceptions() -> Result<(), Error> {
        let mapping_rules = SourceMappingRules {
            default_prefix: "prefix".to_string(),
            overrides: btreemap! {
                "override".to_string() => vec![
                    "overriden_1".to_string(),
                    "overriden_2".to_string(),
                ]
            },
            ..Default::default()
        };
        let multi_mover = create_source_to_target_multi_mover(mapping_rules)?;
        assert_eq!(
            multi_mover(&MPath::new("path")?)?,
            vec![MPath::new("prefix/path")?]
        );

        assert_eq!(
            multi_mover(&MPath::new("override/path")?)?,
            vec![
                MPath::new("overriden_1/path")?,
                MPath::new("overriden_2/path")?,
            ]
        );
        Ok(())
    }

    #[test]
    fn test_multi_mover_longest_prefix_first() -> Result<(), Error> {
        let mapping_rules = SourceMappingRules {
            default_prefix: "prefix".to_string(),
            overrides: btreemap! {
                "prefix".to_string() => vec![
                    "prefix_1".to_string(),
                ],
                "prefix/sub".to_string() => vec![
                    "prefix/sub_1".to_string(),
                ]
            },
            ..Default::default()
        };
        let multi_mover = create_source_to_target_multi_mover(mapping_rules)?;
        assert_eq!(
            multi_mover(&MPath::new("prefix/path")?)?,
            vec![MPath::new("prefix_1/path")?]
        );

        assert_eq!(
            multi_mover(&MPath::new("prefix/sub/path")?)?,
            vec![MPath::new("prefix/sub_1/path")?]
        );

        Ok(())
    }

    fn path(p: &str) -> MPath {
        MPath::new(p).unwrap()
    }

    fn verify_minimized(changes: Vec<(&str, Option<()>)>, expected: BTreeMap<&str, Option<()>>) {
        let changes: Vec<_> = changes.into_iter().map(|(p, c)| (path(p), c)).collect();
        let minimized = minimize_file_change_set(changes);
        let expected: SortedVectorMap<MPath, Option<()>> =
            expected.into_iter().map(|(p, c)| (path(p), c)).collect();
        assert_eq!(expected, minimized);
    }

    #[fbinit::test]
    fn test_minimize_file_change_set(_fb: FacebookInit) {
        verify_minimized(
            vec![("a", Some(())), ("a", None)],
            btreemap! { "a" => Some(())},
        );
        verify_minimized(vec![("a", Some(()))], btreemap! { "a" => Some(())});
        verify_minimized(vec![("a", None)], btreemap! { "a" => None});
        // directories are deleted implicitly, so explicit deletes are
        // minimized away
        verify_minimized(
            vec![("a/b", None), ("a/c", None), ("a", Some(()))],
            btreemap! { "a" => Some(()) },
        );
        // files, replaced with a directy at a longer path are not
        // deleted implicitly, so they aren't minimized away
        verify_minimized(
            vec![("a", None), ("a/b", Some(()))],
            btreemap! { "a" => None, "a/b" => Some(()) },
        );
    }

    #[fbinit::test]
    async fn test_rewrite_commit(fb: FacebookInit) -> Result<(), Error> {
        let repo = TestRepoFactory::new()?.build()?;
        let ctx = CoreContext::test_mock(fb);
        let first = CreateCommitContext::new_root(&ctx, &repo)
            .add_file("path", "path")
            .commit()
            .await?;
        let second = CreateCommitContext::new(&ctx, &repo, vec![first])
            .add_file_with_copy_info("pathsecondcommit", "pathsecondcommit", (first, "path"))
            .commit()
            .await?;

        let mapping_rules = SourceMappingRules {
            default_prefix: "prefix".to_string(),
            overrides: btreemap! {
                "path".to_string() => vec![
                    "path_1".to_string(),
                    "path_2".to_string(),
                ]
            },
            ..Default::default()
        };
        let multi_mover = create_source_to_target_multi_mover(mapping_rules)?;

        let first_rewritten_bcs_id =
            test_rewrite_commit_cs_id(&ctx, &repo, first, HashMap::new(), multi_mover.clone())
                .await?;

        let first_rewritten_wc =
            list_working_copy_utf8(&ctx, &repo, first_rewritten_bcs_id).await?;
        assert_eq!(
            first_rewritten_wc,
            hashmap! {
                MPath::new("path_1")? => "path".to_string(),
                MPath::new("path_2")? => "path".to_string(),
            }
        );

        let second_rewritten_bcs_id = test_rewrite_commit_cs_id(
            &ctx,
            &repo,
            second,
            hashmap! {
                first => first_rewritten_bcs_id
            },
            multi_mover,
        )
        .await?;

        let second_bcs = second_rewritten_bcs_id
            .load(&ctx, &repo.get_blobstore())
            .await?;
        let maybe_copy_from = second_bcs
            .file_changes_map()
            .get(&MPath::new("prefix/pathsecondcommit")?)
            .ok_or_else(|| anyhow!("path not found"))?
            .as_ref()
            .ok_or_else(|| anyhow!("path_is_deleted"))?
            .copy_from()
            .cloned();

        assert_eq!(
            maybe_copy_from,
            Some((MPath::new("path_1")?, first_rewritten_bcs_id))
        );

        let second_rewritten_wc =
            list_working_copy_utf8(&ctx, &repo, second_rewritten_bcs_id).await?;
        assert_eq!(
            second_rewritten_wc,
            hashmap! {
                MPath::new("path_1")? => "path".to_string(),
                MPath::new("path_2")? => "path".to_string(),
                MPath::new("prefix/pathsecondcommit")? => "pathsecondcommit".to_string(),
            }
        );

        Ok(())
    }

    async fn test_rewrite_commit_cs_id<'a>(
        ctx: &'a CoreContext,
        repo: &'a BlobRepo,
        bcs_id: ChangesetId,
        parents: HashMap<ChangesetId, ChangesetId>,
        multi_mover: MultiMover,
    ) -> Result<ChangesetId, Error> {
        let bcs = bcs_id.load(&ctx, &repo.get_blobstore()).await?;
        let bcs = bcs.into_mut();

        let maybe_rewritten =
            rewrite_commit(&ctx, bcs, &parents, multi_mover, repo.clone()).await?;
        let rewritten =
            maybe_rewritten.ok_or_else(|| anyhow!("can't rewrite commit {}", bcs_id))?;
        let rewritten = rewritten.freeze()?;

        save_bonsai_changesets(vec![rewritten.clone()], ctx.clone(), repo.clone()).await?;

        Ok(rewritten.get_changeset_id())
    }
}
