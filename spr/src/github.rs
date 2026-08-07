/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use graphql_client::{GraphQLQuery, Response};
use serde::Deserialize;

use crate::{
    error::{Error, Result, ResultExt},
    message::{MessageSection, MessageSectionsMap, build_github_body, parse_message},
};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

mod stacks;

pub use stacks::{
    Stack, StackApiError, StackBase, StackGitRef, StackPullRequest, StackPullRequestState,
    StackResult, UnstackOutcome,
};

#[derive(Clone)]
pub struct GitHub {
    config: crate::config::Config,
    repo_path: PathBuf,
    graphql_client: reqwest::Client,
}

#[derive(Debug, Clone)]
pub struct PullRequest {
    pub number: u64,
    /// GitHub's own identifier for the pull request, which is what its GraphQL
    /// API takes where the REST API takes [`PullRequest::number`].
    pub node_id: String,
    pub state: PullRequestState,
    pub title: String,
    pub body: Option<String>,
    pub sections: MessageSectionsMap,
    pub base: GitHubBranch,
    pub head: GitHubBranch,
    pub base_oid: git2::Oid,
    pub head_oid: git2::Oid,
    pub merge_commit: Option<git2::Oid>,
    pub reviewers: HashMap<String, ReviewStatus>,
    pub review_status: Option<ReviewStatus>,
}

/// An open pull request that sits on top of another one, as far as the base
/// branch it targets goes.
///
/// Taking the pull request below out of the stack — landing it or closing it —
/// makes that base branch obsolete, and under a linear `spr.baseStrategy` also
/// doomed, since it is the head branch of the pull request below.
#[derive(Debug, Clone)]
pub struct StackedPullRequest {
    pub number: u64,
    /// The branch the pull request is based on right now.
    pub base: GitHubBranch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewStatus {
    Requested,
    Approved,
    Rejected,
}

#[derive(serde::Serialize, Default, Debug)]
pub struct PullRequestUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<PullRequestState>,
}

impl PullRequestUpdate {
    pub fn is_empty(&self) -> bool {
        self.title.is_none() && self.body.is_none() && self.base.is_none() && self.state.is_none()
    }

    pub fn update_message(&mut self, pull_request: &PullRequest, message: &MessageSectionsMap) {
        let title = message.get(&MessageSection::Title);
        if title.is_some() && title != Some(&pull_request.title) {
            self.title = title.cloned();
        }

        let body = build_github_body(message);
        if pull_request.body.as_ref() != Some(&body) {
            self.body = Some(body);
        }
    }
}

#[derive(serde::Serialize, Default, Debug)]
pub struct PullRequestRequestReviewers {
    pub reviewers: Vec<String>,
    pub team_reviewers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PullRequestState {
    Open,
    Closed,
}

#[derive(serde::Deserialize, Debug, Clone)]
pub struct UserWithName {
    pub login: String,
    pub name: Option<String>,
    #[serde(default)]
    pub is_collaborator: bool,
}

/// GitHub's verdict on whether a pull request satisfies what its base branch
/// requires: required checks, required reviews, and rules.
///
/// This is deliberately coarser than GitHub's `MergeStateStatus`, which
/// distinguishes several ways of being ready that landing does not care to
/// tell apart. What landing needs to know is only whether something stands in
/// the way — GitHub does not report *which* requirement is unmet in any case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeRequirements {
    /// Something the base branch requires is unmet.
    Unmet,
    /// Nothing required stands in the way.
    Met,
    /// GitHub has not worked out an answer yet, or gave one this build does
    /// not know. Landing waits for it rather than guessing either way.
    Unknown,
}

#[derive(Debug, Clone)]
pub struct PullRequestMergeability {
    pub base: GitHubBranch,
    pub head_oid: git2::Oid,
    pub mergeable: Option<bool>,
    pub merge_requirements: MergeRequirements,
    pub merge_commit: Option<git2::Oid>,
}

impl PullRequestMergeability {
    /// Whether GitHub has settled on a verdict about the base branch's
    /// requirements yet.
    ///
    /// It works this out lazily, and retargeting a pull request sends it back
    /// to undecided, so a caller that means to act on the verdict has to wait
    /// for one to arrive.
    pub fn requirements_known(&self) -> bool {
        self.merge_requirements != MergeRequirements::Unknown
    }

    /// Whether GitHub reports something the base branch requires as unmet.
    ///
    /// False while the verdict is still [`MergeRequirements::Unknown`], so
    /// pair it with [`Self::requirements_known`] rather than reading a `false`
    /// here as permission to merge.
    pub fn requirements_unmet(&self) -> bool {
        self.merge_requirements == MergeRequirements::Unmet
    }
}

/// Read [`MergeRequirements`] off GitHub's `mergeStateStatus`.
///
/// The doubtful case is `BEHIND`, which GitHub gives when the head ref is out
/// of date. That only bars merging when the base branch requires branches to
/// be up to date, and nothing in the response says whether it does. `jj spr
/// list` resolves that doubt towards reporting nothing, because a column that
/// wrongly claims a pull request cannot land is worse than a quiet one. Here
/// the doubt resolves the other way: a land refused in error costs a rebase or
/// a `--force`, while a land allowed in error cannot be taken back.
fn merge_requirements(
    status: &pull_request_mergeability_query::MergeStateStatus,
) -> MergeRequirements {
    use pull_request_mergeability_query::MergeStateStatus as Status;

    match status {
        // Something required is unmet; GitHub does not say what.
        Status::BLOCKED => MergeRequirements::Unmet,
        // A draft is not offered for merging at all.
        Status::DRAFT => MergeRequirements::Unmet,
        // Conflicting. `mergeable` reports this too, and reports it better,
        // but a land must not proceed on it either way.
        Status::DIRTY => MergeRequirements::Unmet,
        // Out of date — see above.
        Status::BEHIND => MergeRequirements::Unmet,
        // Ready. `HAS_HOOKS` is `CLEAN` with pre-receive hooks configured, and
        // `UNSTABLE` is GitHub's word for a failing check that nothing
        // requires — neither stands in the way of a merge.
        Status::CLEAN | Status::HAS_HOOKS | Status::UNSTABLE => MergeRequirements::Met,
        Status::UNKNOWN => MergeRequirements::Unknown,
        // A status this build does not know is not evidence of readiness.
        Status::Other(_) => MergeRequirements::Unknown,
    }
}

/// The merge queue GitHub keeps for one branch.
///
/// Its existence is the whole of what a caller has to know to decide how to
/// land: a branch that has one takes no other kind of merge. The two fields are
/// for saying so to whoever asked — where the queue is, and how long joining
/// the back of it looks like taking.
#[derive(Debug, Clone)]
pub struct MergeQueue {
    pub url: String,
    /// Seconds GitHub estimates a Pull Request queued now would wait, where it
    /// will estimate at all. It gives no answer for an empty queue, or for one
    /// it has not seen enough of to guess from.
    pub next_entry_estimated_time_to_merge: Option<i64>,
}

/// One Pull Request's place in a merge queue.
#[derive(Debug, Clone)]
pub struct MergeQueueEntry {
    /// Where GitHub says the entry sits in the queue, in GitHub's own
    /// numbering. Passed through as given: the API documents the field as "the
    /// position of this entry in the queue" and does not say what it counts
    /// from, so renumbering it would be inventing a fact.
    pub position: i64,
    /// Seconds GitHub estimates this entry will wait, where it will estimate at
    /// all.
    pub estimated_time_to_merge: Option<i64>,
}

/// What has become of a Pull Request that was put in a merge queue.
///
/// The three fields together say which of the three things has happened, and no
/// one of them says it alone. An entry and no merge commit is a Pull Request
/// still waiting. A merge commit is one the queue merged. Neither an entry nor
/// a merge commit means the queue let go of it without merging — its checks
/// failed on the merged result, or somebody took it out — and `state` tells
/// apart a Pull Request that is still open, and so could be queued again, from
/// one that was closed.
#[derive(Debug, Clone)]
pub struct QueuedPullRequest {
    pub state: PullRequestState,
    pub merge_commit: Option<git2::Oid>,
    pub entry: Option<MergeQueueEntry>,
}

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "src/gql/schema.docs.graphql",
    query_path = "src/gql/pullrequest_query.graphql",
    response_derives = "Debug"
)]
pub struct PullRequestQuery;
type GitObjectID = String;
// Named for GitHub's `URI` scalar, which is how `graphql_client` looks it up.
#[allow(clippy::upper_case_acronyms)]
type URI = String;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "src/gql/schema.docs.graphql",
    query_path = "src/gql/pullrequest_mergeability_query.graphql",
    response_derives = "Debug"
)]
pub struct PullRequestMergeabilityQuery;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "src/gql/schema.docs.graphql",
    query_path = "src/gql/open_pull_request_branches.graphql",
    response_derives = "Debug"
)]
pub struct OpenPullRequestBranchesQuery;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "src/gql/schema.docs.graphql",
    query_path = "src/gql/pull_requests_with_base.graphql",
    response_derives = "Debug"
)]
pub struct PullRequestsWithBaseQuery;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "src/gql/schema.docs.graphql",
    query_path = "src/gql/merge_queue_query.graphql",
    response_derives = "Debug"
)]
pub struct MergeQueueQuery;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "src/gql/schema.docs.graphql",
    query_path = "src/gql/enqueue_pull_request_mutation.graphql",
    response_derives = "Debug"
)]
pub struct EnqueuePullRequestMutation;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "src/gql/schema.docs.graphql",
    query_path = "src/gql/merge_queue_entry_query.graphql",
    response_derives = "Debug"
)]
pub struct MergeQueueEntryQuery;

impl GitHub {
    pub fn new(
        config: crate::config::Config,
        repo_path: PathBuf,
        graphql_client: reqwest::Client,
    ) -> Self {
        Self {
            config,
            repo_path,
            graphql_client,
        }
    }

    pub async fn get_github_user(login: String) -> Result<UserWithName> {
        octocrab::instance()
            .get::<UserWithName, _, _>(format!("/users/{}", login), None::<&()>)
            .await
            .map_err(Error::from)
    }

    pub async fn get_github_team(
        owner: String,
        team: String,
    ) -> Result<octocrab::models::teams::Team> {
        octocrab::instance()
            .teams(owner)
            .get(team)
            .await
            .map_err(Error::from)
    }

    pub async fn get_pull_request(self, number: u64) -> Result<PullRequest> {
        let GitHub {
            config,
            repo_path,
            graphql_client,
        } = self;
        let repo_path = repo_path.to_str().unwrap();

        let variables = pull_request_query::Variables {
            name: config.repo.clone(),
            owner: config.owner.clone(),
            number: number as i64,
        };
        let request_body = PullRequestQuery::build_query(variables);
        let res = graphql_client
            .post("https://api.github.com/graphql")
            .json(&request_body)
            .send()
            .await?;
        let response_body: Response<pull_request_query::ResponseData> = res.json().await?;

        if let Some(errors) = response_body.errors {
            let error = Err(Error::new(format!("fetching PR #{number} failed")));
            return errors
                .into_iter()
                .fold(error, |err, e| err.context(e.to_string()));
        }

        let pr = response_body
            .data
            .ok_or_else(|| Error::new("failed to fetch PR"))?
            .repository
            .ok_or_else(|| Error::new("failed to find repository"))?
            .pull_request
            .ok_or_else(|| Error::new("failed to find PR"))?;

        let base = config.new_github_branch_from_ref(&pr.base_ref_name)?;
        let head = config.new_github_branch_from_ref(&pr.head_ref_name)?;

        // Fetch refs from remote using git (since we're in a colocated repo).
        //
        // Forced, because the head and base commits below are read back out of
        // the local refs these write: a fetch that declined to move one because
        // the remote branch no longer descends from it would leave this
        // reporting a commit the branch has moved off. That happens whenever
        // something rewrote the branch — jj-spr itself under
        // `spr.baseStrategy = linear-rebase`, or GitHub when a stack is merged
        // from its interface — and everything downstream would then be deciding
        // what to push from history GitHub has already dropped.
        let _fetch_result = tokio::process::Command::new("git")
            .args([
                "--git-dir",
                repo_path,
                "fetch",
                "--no-write-fetch-head",
                &config.remote_name,
                &format!("+{}:{}", head.on_github(), head.local()),
                &format!("+{}:{}", base.on_github(), base.local()),
            ])
            .output()
            .await;

        // Convert branch refs to OIDs
        let base_oid = if let Ok(output) = tokio::process::Command::new("git")
            .args(["--git-dir", repo_path, "rev-parse", base.local()])
            .output()
            .await
        {
            if output.status.success() {
                let oid_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
                git2::Oid::from_str(&oid_str).unwrap_or(git2::Oid::zero())
            } else {
                git2::Oid::zero()
            }
        } else {
            git2::Oid::zero()
        };

        let head_oid = if let Ok(output) = tokio::process::Command::new("git")
            .args(["--git-dir", repo_path, "rev-parse", head.local()])
            .output()
            .await
        {
            if output.status.success() {
                let oid_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
                git2::Oid::from_str(&oid_str).unwrap_or(git2::Oid::zero())
            } else {
                git2::Oid::zero()
            }
        } else {
            git2::Oid::zero()
        };

        let mut sections = parse_message(&pr.body, MessageSection::Summary);

        let title = pr.title.trim().to_string();
        sections.insert(
            MessageSection::Title,
            if title.is_empty() {
                String::from("(untitled)")
            } else {
                title
            },
        );

        sections.insert(MessageSection::PullRequest, config.pull_request_url(number));

        let reviewers: HashMap<String, ReviewStatus> = pr
            .latest_opinionated_reviews
            .iter()
            .flat_map(|all_reviews| &all_reviews.nodes)
            .flatten()
            .flatten()
            .flat_map(|review| {
                let user_name = review.author.as_ref()?.login.clone();
                let status = match review.state {
                    pull_request_query::PullRequestReviewState::APPROVED => ReviewStatus::Approved,
                    pull_request_query::PullRequestReviewState::CHANGES_REQUESTED => {
                        ReviewStatus::Rejected
                    }
                    _ => ReviewStatus::Requested,
                };
                Some((user_name, status))
            })
            .collect();

        let review_status = match pr.review_decision {
            Some(pull_request_query::PullRequestReviewDecision::APPROVED) => {
                Some(ReviewStatus::Approved)
            }
            Some(pull_request_query::PullRequestReviewDecision::CHANGES_REQUESTED) => {
                Some(ReviewStatus::Rejected)
            }
            Some(pull_request_query::PullRequestReviewDecision::REVIEW_REQUIRED) => {
                Some(ReviewStatus::Requested)
            }
            _ => None,
        };

        let requested_reviewers: Vec<String> = pr.review_requests
            .iter()
            .flat_map(|x| &x.nodes)
            .flatten()
            .flatten()
            .flat_map(|x| &x.requested_reviewer)
            .flat_map(|reviewer| {
              type UserType = pull_request_query::PullRequestQueryRepositoryPullRequestReviewRequestsNodesRequestedReviewer;
              match reviewer {
                UserType::User(user) => Some(user.login.clone()),
                UserType::Team(team) => Some(format!("#{}", team.slug)),
                _ => None,
              }
            })
            .chain(reviewers.keys().cloned())
            .collect::<HashSet<String>>() // de-duplicate
            .into_iter()
            .collect();

        sections.insert(
            MessageSection::Reviewers,
            requested_reviewers.iter().fold(String::new(), |out, slug| {
                if out.is_empty() {
                    slug.to_string()
                } else {
                    format!("{}, {}", out, slug)
                }
            }),
        );

        if review_status == Some(ReviewStatus::Approved) {
            sections.insert(
                MessageSection::ReviewedBy,
                reviewers
                    .iter()
                    .filter_map(|(k, v)| {
                        if v == &ReviewStatus::Approved {
                            Some(k)
                        } else {
                            None
                        }
                    })
                    .fold(String::new(), |out, slug| {
                        if out.is_empty() {
                            slug.to_string()
                        } else {
                            format!("{}, {}", out, slug)
                        }
                    }),
            );
        }

        Ok::<_, Error>(PullRequest {
            number: pr.number as u64,
            node_id: pr.id,
            state: match pr.state {
                pull_request_query::PullRequestState::OPEN => PullRequestState::Open,
                _ => PullRequestState::Closed,
            },
            title: pr.title,
            body: Some(pr.body),
            sections,
            base,
            head,
            base_oid,
            head_oid,
            reviewers,
            review_status,
            merge_commit: pr
                .merge_commit
                .and_then(|sha| git2::Oid::from_str(&sha.oid).ok()),
        })
    }

    pub async fn create_pull_request(
        &self,
        message: &MessageSectionsMap,
        base_ref_name: String,
        head_ref_name: String,
        draft: bool,
    ) -> Result<u64> {
        let number = octocrab::instance()
            .pulls(self.config.owner.clone(), self.config.repo.clone())
            .create(
                message
                    .get(&MessageSection::Title)
                    .unwrap_or(&String::new()),
                head_ref_name,
                base_ref_name,
            )
            .body(build_github_body(message))
            .draft(Some(draft))
            .send()
            .await?
            .number;

        Ok(number)
    }

    pub async fn update_pull_request(&self, number: u64, updates: PullRequestUpdate) -> Result<()> {
        octocrab::instance()
            .patch::<octocrab::models::pulls::PullRequest, _, _>(
                format!(
                    "/repos/{}/{}/pulls/{}",
                    self.config.owner, self.config.repo, number
                ),
                Some(&updates),
            )
            .await?;

        Ok(())
    }

    /// Set the base branch of pull request `number`, and report the base branch
    /// GitHub has for it afterwards.
    ///
    /// GitHub answers the request with the updated pull request, so the caller
    /// learns whether the change landed without asking again.
    async fn set_pull_request_base(
        &self,
        number: u64,
        base_branch: &GitHubBranch,
    ) -> Result<GitHubBranch> {
        let updated = octocrab::instance()
            .patch::<octocrab::models::pulls::PullRequest, _, _>(
                format!(
                    "/repos/{}/{}/pulls/{}",
                    self.config.owner, self.config.repo, number
                ),
                Some(&PullRequestUpdate {
                    base: Some(base_branch.branch_name().to_string()),
                    ..Default::default()
                }),
            )
            .await?;

        self.config
            .new_github_branch_from_ref(&updated.base.ref_field)
    }

    /// Point pull request `number` at `new_base` and delete `old_base`, the
    /// base branch it targeted until now.
    ///
    /// GitHub closes a pull request whose base branch is deleted, so the branch
    /// only goes away once GitHub has confirmed the retargeting. Only a base
    /// branch jj-spr generated for this purpose is deleted: any other is either
    /// a branch someone wants to keep, or — under a linear `spr.baseStrategy` —
    /// the head branch of the pull request below, and deleting that would close
    /// *it*.
    ///
    /// Returns whether the old base branch was deleted from the remote.
    pub async fn retarget_pull_request(
        &self,
        number: u64,
        new_base: &GitHubBranch,
        old_base: &GitHubBranch,
    ) -> Result<bool> {
        let updated_base = self.set_pull_request_base(number, new_base).await?;

        if updated_base.branch_name() != new_base.branch_name() {
            return Err(Error::new(format!(
                "GitHub reports Pull Request #{number} targets '{}', not '{}'",
                updated_base.branch_name(),
                new_base.branch_name()
            )));
        }

        // Retargeting a pull request at the branch it already points at is not
        // a reason to delete that branch — which is to say, to close it.
        if old_base.branch_name() == new_base.branch_name()
            || !self.config.is_synthetic_base_branch(old_base.branch_name())
        {
            return Ok(false);
        }

        self.delete_remote_branch(old_base).await
    }

    /// [`Self::retarget_pull_request`] to the master branch.
    pub async fn retarget_to_master_branch(
        &self,
        number: u64,
        old_base: &GitHubBranch,
    ) -> Result<bool> {
        self.retarget_pull_request(number, &self.config.master_ref, old_base)
            .await
    }

    /// Delete `branch` from the remote, reporting whether the remote had it.
    ///
    /// A failure here is not an error: the branch may have been deleted
    /// already, either by someone else or by GitHub itself.
    async fn delete_remote_branch(&self, branch: &GitHubBranch) -> Result<bool> {
        let output = tokio::process::Command::new("git")
            .arg("--git-dir")
            .arg(&self.repo_path)
            .arg("push")
            .arg("--no-verify")
            .arg("--delete")
            .arg("--")
            .arg(&self.config.remote_name)
            .arg(branch.on_github())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .await?;

        Ok(output.status.success())
    }

    pub async fn request_reviewers(
        &self,
        number: u64,
        reviewers: PullRequestRequestReviewers,
    ) -> Result<()> {
        #[derive(Deserialize)]
        struct Ignore {}
        let _: Ignore = octocrab::instance()
            .post(
                format!(
                    "/repos/{}/{}/pulls/{}/requested_reviewers",
                    self.config.owner, self.config.repo, number
                ),
                Some(&reviewers),
            )
            .await?;

        Ok(())
    }

    pub async fn get_pull_request_mergeability(
        &self,
        number: u64,
    ) -> Result<PullRequestMergeability> {
        let variables = pull_request_mergeability_query::Variables {
            name: self.config.repo.clone(),
            owner: self.config.owner.clone(),
            number: number as i64,
        };
        let request_body = PullRequestMergeabilityQuery::build_query(variables);
        let res = self
            .graphql_client
            .post("https://api.github.com/graphql")
            .json(&request_body)
            .send()
            .await?;
        let response_body: Response<pull_request_mergeability_query::ResponseData> =
            res.json().await?;

        if let Some(errors) = response_body.errors {
            let error = Err(Error::new(format!(
                "querying PR #{number} mergeability failed"
            )));
            return errors
                .into_iter()
                .fold(error, |err, e| err.context(e.to_string()));
        }

        let pr = response_body
            .data
            .ok_or_else(|| Error::new("failed to fetch PR"))?
            .repository
            .ok_or_else(|| Error::new("failed to find repository"))?
            .pull_request
            .ok_or_else(|| Error::new("failed to find PR"))?;

        Ok::<_, Error>(PullRequestMergeability {
            base: self.config.new_github_branch_from_ref(&pr.base_ref_name)?,
            head_oid: git2::Oid::from_str(&pr.head_ref_oid)?,
            mergeable: match pr.mergeable {
                pull_request_mergeability_query::MergeableState::CONFLICTING => Some(false),
                pull_request_mergeability_query::MergeableState::MERGEABLE => Some(true),
                pull_request_mergeability_query::MergeableState::UNKNOWN => None,
                _ => None,
            },
            merge_requirements: merge_requirements(&pr.merge_state_status),
            merge_commit: pr
                .merge_commit
                .and_then(|sha| git2::Oid::from_str(&sha.oid).ok()),
        })
    }

    /// The open pull requests GitHub has based on `base`.
    ///
    /// This sees pull requests the local repository cannot — see
    /// [`crate::stacked`] for why that matters before a branch is deleted.
    pub async fn get_pull_requests_with_base(
        &self,
        base: &GitHubBranch,
    ) -> Result<Vec<StackedPullRequest>> {
        let mut pull_requests = Vec::new();
        let mut after: Option<String> = None;

        loop {
            let variables = pull_requests_with_base_query::Variables {
                owner: self.config.owner.clone(),
                name: self.config.repo.clone(),
                base_ref_name: base.branch_name().to_string(),
                first: 100,
                after: after.clone(),
            };
            let request_body = PullRequestsWithBaseQuery::build_query(variables);
            let res = self
                .graphql_client
                .post("https://api.github.com/graphql")
                .json(&request_body)
                .send()
                .await?;
            let response_body: Response<pull_requests_with_base_query::ResponseData> =
                res.json().await?;

            if let Some(errors) = response_body.errors {
                let error = Err(Error::new(format!(
                    "fetching the open Pull Requests based on '{}' failed",
                    base.branch_name()
                )));
                return errors
                    .into_iter()
                    .fold(error, |err, e| err.context(e.to_string()));
            }

            let prs = response_body
                .data
                .ok_or_else(|| {
                    Error::new(format!(
                        "failed to fetch the open PRs based on '{}'",
                        base.branch_name()
                    ))
                })?
                .repository
                .ok_or_else(|| Error::new("failed to find repository"))?
                .pull_requests;

            if let Some(nodes) = prs.nodes {
                for node in nodes.into_iter().flatten() {
                    pull_requests.push(StackedPullRequest {
                        number: node.number as u64,
                        base: self
                            .config
                            .new_github_branch_from_ref(&node.base_ref_name)?,
                    });
                }
            }

            if prs.page_info.has_next_page {
                after = prs.page_info.end_cursor;
            } else {
                break;
            }
        }

        Ok(pull_requests)
    }

    /// The merge queue GitHub keeps for `branch_name`, or `None` where it keeps
    /// none.
    ///
    /// `None` is the ordinary answer and not a failure: most branches have no
    /// merge queue, and GitHub says so by returning null for the queue rather
    /// than by refusing the question. A repository jj-spr cannot see at all
    /// still fails, because that is a different thing from a branch without a
    /// queue and a caller reading `None` as "merge it directly" must not be
    /// told it by a lookup that never reached GitHub.
    pub async fn get_merge_queue(&self, branch_name: &str) -> Result<Option<MergeQueue>> {
        let variables = merge_queue_query::Variables {
            name: self.config.repo.clone(),
            owner: self.config.owner.clone(),
            branch: branch_name.to_string(),
        };
        let request_body = MergeQueueQuery::build_query(variables);
        let res = self
            .graphql_client
            .post("https://api.github.com/graphql")
            .json(&request_body)
            .send()
            .await?;
        let response_body: Response<merge_queue_query::ResponseData> = res.json().await?;

        if let Some(errors) = response_body.errors {
            let error = Err(Error::new(format!(
                "querying the merge queue of branch '{branch_name}' failed"
            )));
            return errors
                .into_iter()
                .fold(error, |err, e| err.context(e.to_string()));
        }

        let merge_queue = response_body
            .data
            .ok_or_else(|| Error::new("failed to fetch the merge queue"))?
            .repository
            .ok_or_else(|| Error::new("failed to find repository"))?
            .merge_queue;

        Ok(merge_queue.map(|queue| MergeQueue {
            url: queue.url,
            next_entry_estimated_time_to_merge: queue.next_entry_estimated_time_to_merge,
        }))
    }

    /// Put the Pull Request `node_id` names in the merge queue of its base
    /// branch, and hand back the entry GitHub made for it.
    ///
    /// `head_oid` is the commit this enqueue is about. GitHub refuses the
    /// enqueue where the Pull Request has moved on from it, which is what makes
    /// this safe to ask for on the strength of a mergeability verdict taken a
    /// moment earlier: the queue merges later, on its own schedule, so a push
    /// that arrives in between would otherwise be queued by a land that never
    /// looked at it.
    pub async fn enqueue_pull_request(
        &self,
        node_id: &str,
        head_oid: git2::Oid,
    ) -> Result<MergeQueueEntry> {
        let variables = enqueue_pull_request_mutation::Variables {
            pull_request_id: node_id.to_string(),
            expected_head_oid: format!("{}", head_oid),
        };
        let request_body = EnqueuePullRequestMutation::build_query(variables);
        let res = self
            .graphql_client
            .post("https://api.github.com/graphql")
            .json(&request_body)
            .send()
            .await?;
        let response_body: Response<enqueue_pull_request_mutation::ResponseData> =
            res.json().await?;

        if let Some(errors) = response_body.errors {
            let error = Err(Error::new(
                "adding the Pull Request to the merge queue failed",
            ));
            return errors
                .into_iter()
                .fold(error, |err, e| err.context(e.to_string()));
        }

        // GitHub answers a successful mutation with the entry it made. An
        // answer without one means the mutation reported no error and did
        // nothing, which is not something to report as queued.
        response_body
            .data
            .and_then(|data| data.enqueue_pull_request)
            .and_then(|payload| payload.merge_queue_entry)
            .map(|entry| MergeQueueEntry {
                position: entry.position,
                estimated_time_to_merge: entry.estimated_time_to_merge,
            })
            .ok_or_else(|| {
                Error::new("GitHub did not say the Pull Request was added to the merge queue")
            })
    }

    /// Where Pull Request `number` has got to in the merge queue it was put in.
    ///
    /// Asked repeatedly by a land that waits, so it fetches what tells the
    /// three outcomes apart and nothing else.
    pub async fn get_queued_pull_request(&self, number: u64) -> Result<QueuedPullRequest> {
        let variables = merge_queue_entry_query::Variables {
            name: self.config.repo.clone(),
            owner: self.config.owner.clone(),
            number: number as i64,
        };
        let request_body = MergeQueueEntryQuery::build_query(variables);
        let res = self
            .graphql_client
            .post("https://api.github.com/graphql")
            .json(&request_body)
            .send()
            .await?;
        let response_body: Response<merge_queue_entry_query::ResponseData> = res.json().await?;

        if let Some(errors) = response_body.errors {
            let error = Err(Error::new(format!(
                "querying PR #{number} in the merge queue failed"
            )));
            return errors
                .into_iter()
                .fold(error, |err, e| err.context(e.to_string()));
        }

        let pr = response_body
            .data
            .ok_or_else(|| Error::new("failed to fetch PR"))?
            .repository
            .ok_or_else(|| Error::new("failed to find repository"))?
            .pull_request
            .ok_or_else(|| Error::new("failed to find PR"))?;

        Ok(QueuedPullRequest {
            state: match pr.state {
                merge_queue_entry_query::PullRequestState::OPEN => PullRequestState::Open,
                _ => PullRequestState::Closed,
            },
            merge_commit: pr
                .merge_commit
                .and_then(|sha| git2::Oid::from_str(&sha.oid).ok()),
            entry: pr.merge_queue_entry.map(|entry| MergeQueueEntry {
                position: entry.position,
                estimated_time_to_merge: entry.estimated_time_to_merge,
            }),
        })
    }

    pub async fn get_open_pr_branch_names(&self) -> Result<HashSet<String>> {
        let mut branch_names = HashSet::new();
        let mut after: Option<String> = None;

        loop {
            let variables = open_pull_request_branches_query::Variables {
                owner: self.config.owner.clone(),
                name: self.config.repo.clone(),
                first: 100,
                after: after.clone(),
            };
            let request_body = OpenPullRequestBranchesQuery::build_query(variables);
            let res = self
                .graphql_client
                .post("https://api.github.com/graphql")
                .json(&request_body)
                .send()
                .await?;
            let response_body: Response<open_pull_request_branches_query::ResponseData> =
                res.json().await?;

            if let Some(errors) = response_body.errors {
                let error = Err(Error::new("fetching open PR branches failed".to_string()));
                return errors
                    .into_iter()
                    .fold(error, |err, e| err.context(e.to_string()));
            }

            let prs = response_body
                .data
                .ok_or_else(|| Error::new("failed to fetch open PRs"))?
                .repository
                .ok_or_else(|| Error::new("failed to find repository"))?
                .pull_requests;

            if let Some(nodes) = prs.nodes {
                for node in nodes.into_iter().flatten() {
                    branch_names.insert(node.head_ref_name);
                    branch_names.insert(node.base_ref_name);
                }
            }

            if prs.page_info.has_next_page {
                after = prs.page_info.end_cursor;
            } else {
                break;
            }
        }

        Ok(branch_names)
    }
}

#[derive(Debug, Clone)]
pub struct GitHubBranch {
    ref_on_github: String,
    ref_local: String,
    is_master_branch: bool,
}

impl GitHubBranch {
    pub fn new_from_ref(ghref: &str, remote_name: &str, master_branch_name: &str) -> Result<Self> {
        let ref_on_github = if ghref.starts_with("refs/heads/") {
            ghref.to_string()
        } else if ghref.starts_with("refs/") {
            return Err(Error::new(format!(
                "Ref '{ghref}' does not refer to a branch"
            )));
        } else {
            format!("refs/heads/{ghref}")
        };

        // The branch name is `ref_on_github` with the `refs/heads/` prefix
        // (length 11) removed
        let branch_name = &ref_on_github[11..];
        let ref_local = format!("refs/remotes/{remote_name}/{branch_name}");
        let is_master_branch = branch_name == master_branch_name;

        Ok(Self {
            ref_on_github,
            ref_local,
            is_master_branch,
        })
    }

    pub fn new_from_branch_name(
        branch_name: &str,
        remote_name: &str,
        master_branch_name: &str,
    ) -> Self {
        Self {
            ref_on_github: format!("refs/heads/{branch_name}"),
            ref_local: format!("refs/remotes/{remote_name}/{branch_name}"),
            is_master_branch: branch_name == master_branch_name,
        }
    }

    pub fn on_github(&self) -> &str {
        &self.ref_on_github
    }

    pub fn local(&self) -> &str {
        &self.ref_local
    }

    pub fn is_master_branch(&self) -> bool {
        self.is_master_branch
    }

    pub fn branch_name(&self) -> &str {
        // The branch name is `ref_on_github` with the `refs/heads/` prefix
        // (length 11) removed
        &self.ref_on_github[11..]
    }
}

#[cfg(test)]
mod tests {
    // Note this useful idiom: importing names from outer (for mod tests) scope.
    use super::*;

    /// The statuses that mean GitHub is holding the pull request back.
    #[test]
    fn unmet_requirements_are_read_off_merge_state_status() {
        use pull_request_mergeability_query::MergeStateStatus as Status;

        for status in [
            Status::BLOCKED,
            Status::DRAFT,
            Status::DIRTY,
            Status::BEHIND,
        ] {
            assert_eq!(
                merge_requirements(&status),
                MergeRequirements::Unmet,
                "expected {status:?} to be treated as an unmet requirement"
            );
        }
    }

    /// `UNSTABLE` belongs here rather than above: it is GitHub's word for a
    /// failing check that the base branch does not require, which it will
    /// merge quite happily.
    #[test]
    fn met_requirements_are_read_off_merge_state_status() {
        use pull_request_mergeability_query::MergeStateStatus as Status;

        for status in [Status::CLEAN, Status::HAS_HOOKS, Status::UNSTABLE] {
            assert_eq!(
                merge_requirements(&status),
                MergeRequirements::Met,
                "expected {status:?} to be treated as met"
            );
        }
    }

    /// GitHub computes this lazily, so a pull request it has not looked at yet
    /// — or has just had its base rewritten — answers `UNKNOWN`.
    #[test]
    fn uncomputed_requirements_are_unknown() {
        assert_eq!(
            merge_requirements(&pull_request_mergeability_query::MergeStateStatus::UNKNOWN),
            MergeRequirements::Unknown
        );
    }

    /// A status added to GitHub's schema after this build must not be read as
    /// permission to merge.
    #[test]
    fn unrecognised_status_is_not_met() {
        assert_eq!(
            merge_requirements(&pull_request_mergeability_query::MergeStateStatus::Other(
                "SOMETHING_NEW".to_string()
            )),
            MergeRequirements::Unknown
        );
    }

    #[test]
    fn test_new_from_ref_with_branch_name() {
        let r = GitHubBranch::new_from_ref("foo", "github-remote", "masterbranch").unwrap();
        assert_eq!(r.on_github(), "refs/heads/foo");
        assert_eq!(r.local(), "refs/remotes/github-remote/foo");
        assert_eq!(r.branch_name(), "foo");
        assert!(!r.is_master_branch());
    }

    #[test]
    fn test_new_from_ref_with_master_branch_name() {
        let r =
            GitHubBranch::new_from_ref("masterbranch", "github-remote", "masterbranch").unwrap();
        assert_eq!(r.on_github(), "refs/heads/masterbranch");
        assert_eq!(r.local(), "refs/remotes/github-remote/masterbranch");
        assert_eq!(r.branch_name(), "masterbranch");
        assert!(r.is_master_branch());
    }

    #[test]
    fn test_new_from_ref_with_ref_name() {
        let r =
            GitHubBranch::new_from_ref("refs/heads/foo", "github-remote", "masterbranch").unwrap();
        assert_eq!(r.on_github(), "refs/heads/foo");
        assert_eq!(r.local(), "refs/remotes/github-remote/foo");
        assert_eq!(r.branch_name(), "foo");
        assert!(!r.is_master_branch());
    }

    #[test]
    fn test_new_from_ref_with_master_ref_name() {
        let r =
            GitHubBranch::new_from_ref("refs/heads/masterbranch", "github-remote", "masterbranch")
                .unwrap();
        assert_eq!(r.on_github(), "refs/heads/masterbranch");
        assert_eq!(r.local(), "refs/remotes/github-remote/masterbranch");
        assert_eq!(r.branch_name(), "masterbranch");
        assert!(r.is_master_branch());
    }

    #[test]
    fn test_new_from_branch_name() {
        let r = GitHubBranch::new_from_branch_name("foo", "github-remote", "masterbranch");
        assert_eq!(r.on_github(), "refs/heads/foo");
        assert_eq!(r.local(), "refs/remotes/github-remote/foo");
        assert_eq!(r.branch_name(), "foo");
        assert!(!r.is_master_branch());
    }

    #[test]
    fn test_new_from_master_branch_name() {
        let r = GitHubBranch::new_from_branch_name("masterbranch", "github-remote", "masterbranch");
        assert_eq!(r.on_github(), "refs/heads/masterbranch");
        assert_eq!(r.local(), "refs/remotes/github-remote/masterbranch");
        assert_eq!(r.branch_name(), "masterbranch");
        assert!(r.is_master_branch());
    }

    #[test]
    fn test_new_from_ref_with_edge_case_ref_name() {
        let r = GitHubBranch::new_from_ref(
            "refs/heads/refs/heads/foo",
            "github-remote",
            "masterbranch",
        )
        .unwrap();
        assert_eq!(r.on_github(), "refs/heads/refs/heads/foo");
        assert_eq!(r.local(), "refs/remotes/github-remote/refs/heads/foo");
        assert_eq!(r.branch_name(), "refs/heads/foo");
        assert!(!r.is_master_branch());
    }

    #[test]
    fn test_new_from_edge_case_branch_name() {
        let r =
            GitHubBranch::new_from_branch_name("refs/heads/foo", "github-remote", "masterbranch");
        assert_eq!(r.on_github(), "refs/heads/refs/heads/foo");
        assert_eq!(r.local(), "refs/remotes/github-remote/refs/heads/foo");
        assert_eq!(r.branch_name(), "refs/heads/foo");
        assert!(!r.is_master_branch());
    }
}
