/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use borrowed::borrowed;
use commit_cloud::ctx::CommitCloudContext;
use commit_cloud::CommitCloudRef;
use commit_cloud::Phase;
use commit_graph::CommitGraphRef;
use edenapi_types::cloud::CloudShareWorkspaceRequest;
use edenapi_types::cloud::WorkspaceSharingData;
use edenapi_types::GetReferencesParams;
use edenapi_types::GetSmartlogParams;
use edenapi_types::HgId;
use edenapi_types::ReferencesData;
use edenapi_types::SmartlogData;
use edenapi_types::UpdateArchiveParams;
use edenapi_types::UpdateReferencesParams;
use edenapi_types::WorkspaceData;
use futures::TryStreamExt;
use futures_util::future::try_join_all;
use mononoke_api::ChangesetContext;
use mononoke_api::ChangesetSpecifier;
use mononoke_api::MononokeError;
use phases::PhasesRef;

use crate::HgRepoContext;
impl HgRepoContext {
    pub async fn cloud_workspace(
        &self,
        workspace: &str,
        reponame: &str,
    ) -> Result<WorkspaceData, MononokeError> {
        let mut cc_ctx = CommitCloudContext::new(workspace, reponame)?;
        let authz = self.repo_ctx().authorization_context();
        authz
            .require_commitcloud_operation(self.ctx(), &self.repo_ctx().repo(), &mut cc_ctx, "read")
            .await?;
        Ok(self
            .repo_ctx()
            .repo()
            .commit_cloud()
            .get_workspace(&cc_ctx)
            .await?)
    }

    pub async fn cloud_workspaces(
        &self,
        prefix: &str,
        reponame: &str,
    ) -> Result<Vec<WorkspaceData>, MononokeError> {
        Ok(self
            .repo_ctx()
            .repo()
            .commit_cloud()
            .get_workspaces(prefix, reponame)
            .await?)
    }

    pub async fn cloud_references(
        &self,
        params: &GetReferencesParams,
    ) -> Result<ReferencesData, MononokeError> {
        let mut ctx = CommitCloudContext::new(&params.workspace, &params.reponame)?;
        let authz = self.repo_ctx().authorization_context();
        authz
            .require_commitcloud_operation(self.ctx(), &self.repo_ctx().repo(), &mut ctx, "read")
            .await?;
        let cc_ctx = CommitCloudContext::new(&params.workspace, &params.reponame)?;
        Ok(self
            .repo_ctx()
            .repo()
            .commit_cloud()
            .get_references(&cc_ctx, params)
            .await?)
    }

    pub async fn cloud_update_references(
        &self,
        params: &UpdateReferencesParams,
    ) -> Result<ReferencesData, MononokeError> {
        let mut cc_ctx = CommitCloudContext::new(&params.workspace, &params.reponame)?;
        if params.version == 0 {
            cc_ctx.check_workspace_name()?;
        }

        let authz = self.repo_ctx().authorization_context();
        authz
            .require_commitcloud_operation(
                self.ctx(),
                &self.repo_ctx().repo(),
                &mut cc_ctx,
                "write",
            )
            .await?;

        Ok(self
            .repo_ctx()
            .repo()
            .commit_cloud()
            .update_references(&cc_ctx, params)
            .await?)
    }

    pub async fn cloud_smartlog(
        &self,
        params: &GetSmartlogParams,
    ) -> Result<SmartlogData, MononokeError> {
        let raw_data = self
            .repo_ctx()
            .repo()
            .commit_cloud()
            .get_smartlog_raw_info(params)
            .await?;
        let hg_ids = raw_data.collapse_into_vec();

        let ctx = self.ctx();
        let repo = self.repo_ctx().repo();
        let cs_ids = self.convert_changeset_ids(hg_ids).await?;

        let public_frontier = repo
            .commit_graph()
            .ancestors_frontier_with(ctx, cs_ids.clone(), |csid| {
                borrowed!(ctx, repo);
                async move {
                    Ok(repo
                        .phases()
                        .get_cached_public(ctx, vec![csid])
                        .await?
                        .contains(&csid))
                }
            })
            .await?;

        let draft_commits_ctx = repo
            .commit_graph()
            .ancestors_difference_stream(ctx, cs_ids, public_frontier.clone())
            .await?
            .map_ok({
                |cs_id| async move {
                    self.repo_ctx()
                        .changeset(ChangesetSpecifier::Bonsai(cs_id))
                        .await
                }
            })
            .map_err(MononokeError::from)
            .try_buffered(100)
            .try_collect::<Vec<Option<ChangesetContext>>>()
            .await?;

        let public_commits_ctx = try_join_all(
            public_frontier
                .into_iter()
                .map(|cs_id| self.repo_ctx().changeset(ChangesetSpecifier::Bonsai(cs_id))),
        )
        .await?;
        let mut nodes = Vec::new();
        let bookmarks = raw_data.local_bookmarks.unwrap_or_default();
        let remote_bookmarks = raw_data.remote_bookmarks.unwrap_or_default();

        for (phase, changesets) in [
            (Phase::Public, public_commits_ctx),
            (Phase::Draft, draft_commits_ctx),
        ] {
            for changeset in changesets.into_iter().flatten() {
                if let Some(hgid) = changeset.hg_id().await? {
                    let parents = changeset.parents().await?;
                    let hg_parents = self
                        .repo_ctx()
                        .many_changeset_hg_ids(parents)
                        .await?
                        .into_iter()
                        .map(|(_, hg_id)| HgId::from(hg_id))
                        .collect();

                    nodes.push(self.repo_ctx().repo().commit_cloud().make_smartlog_node(
                        &hgid,
                        &hg_parents,
                        &changeset.changeset_info().await?,
                        &bookmarks.get(&hgid).cloned(),
                        &remote_bookmarks.get(&hgid).cloned(),
                        &phase,
                    )?)
                }
            }
        }

        Ok(SmartlogData {
            nodes,
            version: None,
            timestamp: None,
        })
    }

    pub async fn cloud_share_workspace(
        &self,
        request: &CloudShareWorkspaceRequest,
    ) -> Result<WorkspaceSharingData, MononokeError> {
        let mut ctx = CommitCloudContext::new(&request.workspace, &request.reponame)?;

        let authz = self.repo_ctx().authorization_context();
        authz
            .require_commitcloud_operation(
                self.ctx(),
                &self.repo_ctx().repo(),
                &mut ctx,
                "maintainers",
            )
            .await?;

        Ok(self
            .repo_ctx()
            .repo()
            .commit_cloud()
            .share_workspace(&ctx)
            .await?)
    }
    pub async fn cloud_update_archive(
        &self,
        params: &UpdateArchiveParams,
    ) -> Result<String, MononokeError> {
        let mut cc_ctx = CommitCloudContext::new(&params.workspace, &params.reponame)?;

        let authz = self.repo_ctx().authorization_context();
        authz
            .require_commitcloud_operation(
                self.ctx(),
                &self.repo_ctx().repo(),
                &mut cc_ctx,
                "write",
            )
            .await?;

        Ok(self
            .repo_ctx()
            .repo()
            .commit_cloud()
            .update_workspace_archive(&cc_ctx, params.archived)
            .await?)
    }
}
