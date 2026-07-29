/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use crate::error::Result;
use crate::error::ResultExt;
use graphql_client::{GraphQLQuery, Response};
use reqwest;
use tabled::Table;
use tabled::Tabled;
use tabled::settings::Style;

#[allow(clippy::upper_case_acronyms)]
type URI = String;
#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "src/gql/schema.docs.graphql",
    query_path = "src/gql/open_reviews.graphql",
    response_derives = "Debug"
)]
pub struct SearchQuery;

pub async fn list(
    graphql_client: reqwest::Client,
    jj: &crate::jj::Jujutsu,
    config: &crate::config::Config,
) -> Result<()> {
    let variables = search_query::Variables {
        query: format!(
            "repo:{}/{} is:open is:pr author:@me archived:false",
            config.owner, config.repo
        ),
    };
    let request_body = SearchQuery::build_query(variables);
    let res = graphql_client
        .post("https://api.github.com/graphql")
        .json(&request_body)
        .send()
        .await?;
    let response_body: Response<search_query::ResponseData> = res.json().await?;

    // GitHub returns the pull requests in an order of its own, which says
    // nothing about how they depend on one another. The local changes do know,
    // so they decide the order the table is printed in.
    let stacks = jj
        .get_local_pull_request_stacks(config)
        .context("Reading the local change stacks".to_string())?;

    print_pr_info(response_body, &stacks).context("Printing PR info".to_string())
}

#[derive(Tabled)]
struct Row {
    #[tabled(rename = "Stack")]
    stack: String,
    #[tabled(rename = "Merge")]
    merge_status: String,
    #[tabled(rename = "Reviews")]
    review_status: String,
    #[tabled(rename = "Comments")]
    comment_status: String,
    #[tabled(rename = "Description")]
    description: String,
}

/// What stands between a PR and landing, apart from its review.
///
/// The variants are ordered by how much they have to say: the first one that
/// applies is the one worth reporting, since a PR whose branches conflict is
/// not waiting on its tests.
#[derive(Debug, PartialEq, Eq)]
enum MergeStatus {
    /// Marked as a draft, so it is not offered for merging at all.
    Draft,
    /// GitHub has not worked out whether the PR can merge. It computes that
    /// lazily, and asking is what sets it going, so a later run will say.
    Unknown,
    /// The branches conflict.
    Conflicts,
    /// A check that has to pass has not.
    Failing,
    /// A check has failed, but none that the base branch requires, so this
    /// can still land.
    OptionalFailing,
    /// The checks have not finished.
    Running,
    /// Every check passed.
    Passing,
    /// There are no checks to pass.
    NoChecks,
}

impl MergeStatus {
    /// How the status reads in the table.
    fn label(&self) -> console::StyledObject<&'static str> {
        match self {
            MergeStatus::Draft => console::style("Draft").dim(),
            MergeStatus::Unknown => console::style("?").dim(),
            MergeStatus::Conflicts => console::style("Conflicts").red(),
            MergeStatus::Failing => console::style("Failing").red(),
            MergeStatus::OptionalFailing => console::style("Optional failing").yellow(),
            MergeStatus::Running => console::style("Running"),
            MergeStatus::Passing => console::style("Passing").green(),
            MergeStatus::NoChecks => console::style("—").dim(),
        }
    }
}

/// Decide whether a PR is ready to merge.
///
/// Two of GitHub's answers are combined here, because neither is enough on
/// its own. `mergeStateStatus` is GitHub's own verdict, and the only thing
/// that knows which checks the base branch requires — but it is a superset,
/// reporting `BLOCKED` for a PR that is merely unapproved as readily as for
/// one whose tests failed, which would make it useless as a report on checks.
/// The rollup knows the checks but not which of them matter. So the rollup
/// says whether anything is wrong, and `mergeStateStatus` says whether what
/// is wrong actually stands in the way: `UNSTABLE` is GitHub's word for
/// "mergeable, but some check that is not required is unhappy".
fn merge_status(pr: &search_query::SearchQuerySearchNodesOnPullRequest) -> MergeStatus {
    use search_query::{MergeStateStatus, MergeableState, StatusState};

    if pr.is_draft {
        return MergeStatus::Draft;
    }

    match (&pr.mergeable, &pr.merge_state_status) {
        (MergeableState::UNKNOWN, _) | (_, MergeStateStatus::UNKNOWN) => {
            return MergeStatus::Unknown;
        }
        (MergeableState::CONFLICTING, _) | (_, MergeStateStatus::DIRTY) => {
            return MergeStatus::Conflicts;
        }
        // `BEHIND` is deliberately not reported. It only stands in the way of
        // merging when the base branch requires branches to be up to date,
        // and nothing here can tell whether that is so, which would leave the
        // column claiming a PR cannot land when it can.
        _ => {}
    }

    // Nothing rolled up means the PR has no checks, which is not a reason to
    // hold it back.
    let Some(rollup) = &pr.status_check_rollup else {
        return MergeStatus::NoChecks;
    };

    let nothing_required_is_wrong = matches!(pr.merge_state_status, MergeStateStatus::UNSTABLE);

    match rollup.state {
        StatusState::SUCCESS => MergeStatus::Passing,
        StatusState::FAILURE | StatusState::ERROR => {
            if nothing_required_is_wrong {
                MergeStatus::OptionalFailing
            } else {
                MergeStatus::Failing
            }
        }
        StatusState::PENDING | StatusState::EXPECTED => MergeStatus::Running,
        // A state this build does not know is better reported as unknown than
        // as one of the verdicts it is not.
        StatusState::Other(_) => MergeStatus::Unknown,
    }
}

/// Describe where a PR stands with its reviewers.
///
/// `reviewDecision` is authoritative when it has reached a verdict — it
/// already resolves disagreement between reviewers the way GitHub does, so
/// one reviewer requesting changes correctly outweighs another's approval.
/// The individual review states are consulted only when there is no verdict,
/// to report feedback that `reviewDecision` has no way to express.
fn review_status(pr: &search_query::SearchQuerySearchNodesOnPullRequest) -> String {
    match pr.review_decision {
        Some(search_query::PullRequestReviewDecision::APPROVED) => {
            console::style("Accepted").green().to_string()
        }
        Some(search_query::PullRequestReviewDecision::CHANGES_REQUESTED) => {
            console::style("Changes Requested").red().to_string()
        }
        None | Some(search_query::PullRequestReviewDecision::REVIEW_REQUIRED) => {
            let commented = pr
                .reviews
                .iter()
                .flat_map(|r| r.nodes.iter())
                .flatten()
                .flatten()
                .any(|review| {
                    matches!(
                        review.state,
                        search_query::PullRequestReviewState::COMMENTED
                    )
                });

            if commented {
                console::style("Commented").yellow().to_string()
            } else {
                "Pending".to_string()
            }
        }
        Some(search_query::PullRequestReviewDecision::Other(ref d)) => d.clone(),
    }
}

/// Who the conversation on a PR is waiting on.
enum CommentStatus {
    /// A reviewer had the last word, so the PR is waiting on us.
    AwaitingReply,
    /// There is discussion, and we have replied to all of it.
    Replied,
    /// Nobody has commented.
    Quiet,
}

impl CommentStatus {
    fn icon(&self) -> &'static str {
        match self {
            CommentStatus::AwaitingReply => "📬",
            CommentStatus::Replied => "💬",
            CommentStatus::Quiet => "💤",
        }
    }
}

/// Whether we have already dealt with a comment.
///
/// Writing it obviously counts, and so does reacting to it: a thumbs-up is
/// how you acknowledge a comment you have nothing to add to, and a comment we
/// have acknowledged is not one we still owe an answer.
///
/// This is a macro because the query gives top-level comments and review
/// thread comments distinct generated types, despite the two spelling these
/// fields identically.
macro_rules! viewer_handled {
    ($comment:expr) => {
        $comment.viewer_did_author
            || $comment
                .reaction_groups
                .iter()
                .flatten()
                .any(|group| group.viewer_has_reacted)
    };
}

/// Determine who a PR's conversation is waiting on.
///
/// A reviewer is waiting on us if they had the last word in any unresolved
/// review thread, or in the PR's top-level comments, and we have not
/// acknowledged it. GitHub returns comment connections oldest-first, so the
/// last non-minimized node in each is the most recent one.
fn comment_status(pr: &search_query::SearchQuerySearchNodesOnPullRequest) -> CommentStatus {
    let top_level: Vec<_> = pr
        .comments
        .nodes
        .iter()
        .flatten()
        .flatten()
        .filter(|comment| !comment.is_minimized)
        .collect();

    let mut awaiting_reply = top_level.last().is_some_and(|last| !viewer_handled!(last));
    let mut any_discussion = !top_level.is_empty();

    for thread in pr.review_threads.nodes.iter().flatten().flatten() {
        let last_comment = thread
            .comments
            .nodes
            .iter()
            .flatten()
            .flatten()
            .rfind(|comment| !comment.is_minimized);

        let Some(last_comment) = last_comment else {
            continue;
        };
        any_discussion = true;

        // A resolved thread is not waiting on anyone, whoever spoke last.
        if !thread.is_resolved && !viewer_handled!(last_comment) {
            awaiting_reply = true;
        }
    }

    match (awaiting_reply, any_discussion) {
        (true, _) => CommentStatus::AwaitingReply,
        (false, true) => CommentStatus::Replied,
        (false, false) => CommentStatus::Quiet,
    }
}

/// A pull request as the table will show it, before it is known where among
/// the local changes it belongs.
struct PullRequest {
    number: u64,
    merge_status: String,
    review_status: String,
    comment_status: String,
    description: String,
}

/// One block of the table.
struct Group {
    pull_requests: Vec<PullRequest>,
    /// Whether this group is a local stack. The pull requests that no local
    /// change accounts for are gathered into a group of their own, which is
    /// not a stack and so has nothing to draw in the Stack column.
    is_stack: bool,
}

fn print_pr_info(
    response_body: Response<search_query::ResponseData>,
    stacks: &[Vec<u64>],
) -> Result<()> {
    let groups = group_by_stack(collect_pull_requests(response_body), stacks);

    if groups.is_empty() {
        return Ok(());
    }

    let term = console::Term::stdout();
    term.write_line(&build_table(groups).to_string())?;

    Ok(())
}

fn collect_pull_requests(response_body: Response<search_query::ResponseData>) -> Vec<PullRequest> {
    let mut pull_requests: Vec<PullRequest> = Vec::new();

    // A response without data, or without search nodes, means there is
    // simply nothing to list.
    let Some(data) = response_body.data else {
        return pull_requests;
    };
    let Some(search_nodes) = data.search.nodes else {
        return pull_requests;
    };

    for pr in search_nodes.into_iter().flatten() {
        let pr = match pr {
            crate::commands::list::search_query::SearchQuerySearchNodes::PullRequest(pr) => pr,
            _ => continue,
        };

        let merge_status = merge_status(&pr).label().to_string();
        let comment_status = comment_status(&pr).icon().to_string();
        let review_status = review_status(&pr);

        let description = format!(
            "{}\n{}",
            console::style(&pr.title).bold(),
            console::style(&pr.url).dim(),
        );

        pull_requests.push(PullRequest {
            number: pr.number as u64,
            merge_status,
            review_status,
            comment_status,
            description,
        });
    }

    pull_requests
}

/// Sort the pull requests into the stacks the local changes describe.
///
/// The stacks are listed in the order they were given, each in its own group,
/// and a pull request that no local change carries — one landed elsewhere, or
/// opened from another machine — goes into a last group of its own rather than
/// being dropped from a listing that is meant to show everything open.
fn group_by_stack(pull_requests: Vec<PullRequest>, stacks: &[Vec<u64>]) -> Vec<Group> {
    let position: std::collections::HashMap<u64, usize> = pull_requests
        .iter()
        .enumerate()
        .map(|(position, pull_request)| (pull_request.number, position))
        .collect();

    // Taking each pull request out as its stack claims it leaves exactly the
    // ones no stack mentions behind, still in the order GitHub gave them.
    let mut unclaimed: Vec<Option<PullRequest>> = pull_requests.into_iter().map(Some).collect();
    let mut groups: Vec<Group> = Vec::new();

    for stack in stacks {
        let pull_requests: Vec<PullRequest> = stack
            .iter()
            .filter_map(|number| position.get(number))
            .filter_map(|&position| unclaimed[position].take())
            .collect();

        // A stack whose pull requests are all closed has nothing to show.
        if !pull_requests.is_empty() {
            groups.push(Group {
                pull_requests,
                is_stack: true,
            });
        }
    }

    let loose: Vec<PullRequest> = unclaimed.into_iter().flatten().collect();
    if !loose.is_empty() {
        groups.push(Group {
            pull_requests: loose,
            is_stack: false,
        });
    }

    groups
}

fn build_table(groups: Vec<Group>) -> Table {
    // The Stack column is only worth its width when there is more than one
    // group to tell apart: a listing that is one stack from top to bottom
    // already reads in order without it.
    let stack_column_earns_its_place = groups.len() > 1;

    let rows: Vec<Row> = groups
        .into_iter()
        .flat_map(|group| {
            let last = group.pull_requests.len() - 1;
            let is_stack = group.is_stack;
            group
                .pull_requests
                .into_iter()
                .enumerate()
                .map(move |(position, pull_request)| Row {
                    stack: stack_marker(is_stack, position, last),
                    merge_status: pull_request.merge_status,
                    review_status: pull_request.review_status,
                    comment_status: pull_request.comment_status,
                    description: pull_request.description,
                })
        })
        .collect();

    let mut builder = Table::builder(rows);
    if !stack_column_earns_its_place {
        builder.remove_column(0);
    }

    let mut table = builder.build();
    table.with(Style::sharp());

    table
}

/// Draw where a row sits in its stack, in the shape `jj log` gives a branch:
/// the newest change opens the stack and the one nearest master closes it.
fn stack_marker(is_stack: bool, position: usize, last: usize) -> String {
    if !is_stack {
        return String::new();
    }

    let marker = match position {
        0 => "○",
        position if position == last => "╯",
        _ => "│",
    };

    console::style(marker).dim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a search response around one pull request.
    ///
    /// `pr_fields` is spliced into the PullRequest node, so each test spells
    /// out only the part of the payload it cares about, in the same shape the
    /// GraphQL API returns.
    fn response(pr_fields: &str) -> Response<search_query::ResponseData> {
        let json = format!(
            r#"{{"data":{{"search":{{"nodes":[{{
                 "__typename":"PullRequest",
                 "number":1,
                 "title":"a title",
                 "url":"https://github.com/o/r/pull/1",
                 {pr_fields}
               }}]}}}}}}"#
        );
        serde_json::from_str(&json).expect("test payload should match the query's response shape")
    }

    /// The single pull request a test payload describes.
    fn pull_request_node(pr_fields: &str) -> search_query::SearchQuerySearchNodesOnPullRequest {
        let nodes = response(pr_fields)
            .data
            .expect("the payload should have data")
            .search
            .nodes
            .expect("the payload should have search nodes");
        match nodes
            .into_iter()
            .flatten()
            .next()
            .expect("expected exactly one pull request")
        {
            search_query::SearchQuerySearchNodes::PullRequest(pr) => pr,
            _ => panic!("the payload should describe a pull request"),
        }
    }

    /// The fields the Merge column reads, as the query returns them.
    /// `mergeable` and `merge_state` are GitHub's two verdicts, and `rollup`
    /// the rolled-up state of the checks — `None` for a PR that has none.
    fn merge_fields(mergeable: &str, merge_state: &str, rollup: Option<&str>) -> String {
        let rollup = rollup.map_or_else(
            || "null".to_string(),
            |state| format!(r#"{{"state":"{state}"}}"#),
        );
        format!(
            r#""isDraft":false,"mergeable":"{mergeable}",
               "mergeStateStatus":"{merge_state}","statusCheckRollup":{rollup}"#
        )
    }

    /// A pull request with nothing standing between it and merging, for the
    /// tests that are about something else.
    fn ready_to_merge() -> String {
        merge_fields("MERGEABLE", "CLEAN", Some("SUCCESS"))
    }

    /// One visible comment node. `mine` marks one we wrote, `reacted` one we
    /// left a reaction on. A comment nobody has reacted to has no reaction
    /// groups at all.
    fn comment_node(mine: bool, reacted: bool) -> String {
        let groups = if reacted {
            r#"[{"viewerHasReacted":true}]"#
        } else {
            "[]"
        };
        format!(r#"{{"isMinimized":false,"viewerDidAuthor":{mine},"reactionGroups":{groups}}}"#)
    }

    fn comment_nodes(authored: &[bool]) -> String {
        authored
            .iter()
            .map(|mine| comment_node(*mine, false))
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Top-level comments, oldest first. `true` marks one we wrote.
    fn comments(authored: &[bool]) -> String {
        format!(r#""comments":{{"nodes":[{}]}}"#, comment_nodes(authored))
    }

    /// One review thread, its comments oldest first.
    fn thread(resolved: bool, authored: &[bool]) -> String {
        format!(
            r#"{{"isResolved":{resolved},"comments":{{"nodes":[{}]}}}}"#,
            comment_nodes(authored)
        )
    }

    fn threads(threads: &[String]) -> String {
        format!(r#""reviewThreads":{{"nodes":[{}]}}"#, threads.join(","))
    }

    const NO_REVIEWS: &str = r#""reviewDecision":null,"reviews":{"nodes":[]}"#;

    fn comment_icon(pr_fields: &str) -> String {
        let pull_requests =
            collect_pull_requests(response(&format!("{},{pr_fields}", ready_to_merge())));
        assert_eq!(pull_requests.len(), 1, "expected exactly one pull request");
        pull_requests.into_iter().next().unwrap().comment_status
    }

    #[test]
    fn no_comments_at_all_is_quiet() {
        let fields = format!("{NO_REVIEWS},{},{}", comments(&[]), threads(&[]));
        assert_eq!(comment_icon(&fields), CommentStatus::Quiet.icon());
    }

    #[test]
    fn reviewer_with_the_last_top_level_word_awaits_reply() {
        let fields = format!("{NO_REVIEWS},{},{}", comments(&[true, false]), threads(&[]));
        assert_eq!(comment_icon(&fields), CommentStatus::AwaitingReply.icon());
    }

    #[test]
    fn our_own_last_top_level_word_is_replied() {
        let fields = format!("{NO_REVIEWS},{},{}", comments(&[false, true]), threads(&[]));
        assert_eq!(comment_icon(&fields), CommentStatus::Replied.icon());
    }

    #[test]
    fn reviewer_with_the_last_word_in_a_thread_awaits_reply() {
        let fields = format!(
            "{NO_REVIEWS},{},{}",
            comments(&[]),
            threads(&[thread(false, &[true, false])])
        );
        assert_eq!(comment_icon(&fields), CommentStatus::AwaitingReply.icon());
    }

    /// A resolved thread is settled, so it should not ask for a reply even
    /// though a reviewer spoke last in it.
    #[test]
    fn resolved_thread_does_not_await_reply() {
        let fields = format!(
            "{NO_REVIEWS},{},{}",
            comments(&[]),
            threads(&[thread(true, &[true, false])])
        );
        assert_eq!(comment_icon(&fields), CommentStatus::Replied.icon());
    }

    /// Replying on one thread must not mask an open question on another.
    #[test]
    fn replying_to_one_thread_does_not_settle_another() {
        let fields = format!(
            "{NO_REVIEWS},{},{}",
            comments(&[]),
            threads(&[thread(false, &[false, true]), thread(false, &[true, false])])
        );
        assert_eq!(comment_icon(&fields), CommentStatus::AwaitingReply.icon());
    }

    /// Minimized comments are hidden on GitHub, so the last visible comment
    /// decides who spoke last.
    #[test]
    fn minimized_comments_are_ignored() {
        let fields = format!(
            r#"{NO_REVIEWS},"comments":{{"nodes":[
                 {},
                 {{"isMinimized":true,"viewerDidAuthor":false,"reactionGroups":[]}}
               ]}},{}"#,
            comment_node(true, false),
            threads(&[])
        );
        assert_eq!(comment_icon(&fields), CommentStatus::Replied.icon());
    }

    /// A thread of nothing but minimized comments is not discussion.
    #[test]
    fn wholly_minimized_thread_is_quiet() {
        let fields = format!(
            r#"{NO_REVIEWS},{},"reviewThreads":{{"nodes":[
                 {{"isResolved":false,"comments":{{"nodes":[
                   {{"isMinimized":true,"viewerDidAuthor":false,"reactionGroups":[]}}
                 ]}}}}
               ]}}"#,
            comments(&[])
        );
        assert_eq!(comment_icon(&fields), CommentStatus::Quiet.icon());
    }

    /// Reacting to a reviewer's comment acknowledges it, so the PR is no
    /// longer waiting on us even though we never wrote a reply.
    #[test]
    fn our_reaction_to_the_last_top_level_word_is_a_reply() {
        let fields = format!(
            r#"{NO_REVIEWS},"comments":{{"nodes":[{}]}},{}"#,
            comment_node(false, true),
            threads(&[])
        );
        assert_eq!(comment_icon(&fields), CommentStatus::Replied.icon());
    }

    #[test]
    fn our_reaction_settles_an_unresolved_thread() {
        let fields = format!(
            r#"{NO_REVIEWS},{},"reviewThreads":{{"nodes":[
                 {{"isResolved":false,"comments":{{"nodes":[{}]}}}}
               ]}}"#,
            comments(&[]),
            comment_node(false, true)
        );
        assert_eq!(comment_icon(&fields), CommentStatus::Replied.icon());
    }

    /// Someone else's reaction says nothing about whether we have read the
    /// comment, so it must not settle the conversation for us.
    #[test]
    fn a_reaction_that_is_not_ours_still_awaits_reply() {
        let fields = format!(
            r#"{NO_REVIEWS},"comments":{{"nodes":[
                 {{"isMinimized":false,"viewerDidAuthor":false,
                   "reactionGroups":[{{"viewerHasReacted":false}}]}}
               ]}},{}"#,
            threads(&[])
        );
        assert_eq!(comment_icon(&fields), CommentStatus::AwaitingReply.icon());
    }

    /// Acknowledging one comment does not acknowledge the ones that follow it.
    #[test]
    fn a_reaction_does_not_settle_a_later_comment() {
        let fields = format!(
            r#"{NO_REVIEWS},"comments":{{"nodes":[{},{}]}},{}"#,
            comment_node(false, true),
            comment_node(false, false),
            threads(&[])
        );
        assert_eq!(comment_icon(&fields), CommentStatus::AwaitingReply.icon());
    }

    /// The Reviews cell, stripped of styling so tests assert on wording.
    fn review_cell(decision: &str, review_states: &[&str]) -> String {
        let states = review_states
            .iter()
            .map(|state| format!(r#"{{"state":"{state}"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let fields = format!(
            r#"{},"reviewDecision":{decision},"reviews":{{"nodes":[{states}]}},{},{}"#,
            ready_to_merge(),
            comments(&[]),
            threads(&[])
        );
        let pull_requests = collect_pull_requests(response(&fields));
        assert_eq!(pull_requests.len(), 1, "expected exactly one pull request");
        let cell = pull_requests.into_iter().next().unwrap().review_status;
        console::strip_ansi_codes(&cell).into_owned()
    }

    #[test]
    fn approved_pr_is_accepted() {
        assert_eq!(review_cell(r#""APPROVED""#, &["APPROVED"]), "Accepted");
    }

    /// `reviewDecision` stays authoritative: GitHub reports CHANGES_REQUESTED
    /// even though another reviewer approved, and so must we. Deriving the
    /// column from the review states alone would report this as approved.
    #[test]
    fn changes_requested_outweighs_an_approval() {
        assert_eq!(
            review_cell(r#""CHANGES_REQUESTED""#, &["APPROVED", "CHANGES_REQUESTED"]),
            "Changes Requested"
        );
    }

    #[test]
    fn a_commented_review_without_a_verdict_is_commented() {
        assert_eq!(review_cell("null", &["COMMENTED"]), "Commented");
        assert_eq!(
            review_cell(r#""REVIEW_REQUIRED""#, &["COMMENTED"]),
            "Commented"
        );
    }

    #[test]
    fn no_reviews_at_all_is_pending() {
        assert_eq!(review_cell("null", &[]), "Pending");
        assert_eq!(review_cell(r#""REVIEW_REQUIRED""#, &[]), "Pending");
    }

    /// An unsubmitted review is a draft only its author can see, so it is not
    /// yet feedback.
    #[test]
    fn an_unsubmitted_review_is_still_pending() {
        assert_eq!(review_cell("null", &["PENDING"]), "Pending");
    }

    /// Where a PR stands with merging, given GitHub's two verdicts and the
    /// rolled-up state of its checks.
    fn merge_state(mergeable: &str, merge_state: &str, rollup: Option<&str>) -> MergeStatus {
        let fields = format!(
            "{},{NO_REVIEWS},{},{}",
            merge_fields(mergeable, merge_state, rollup),
            comments(&[]),
            threads(&[])
        );
        merge_status(&pull_request_node(&fields))
    }

    #[test]
    fn a_pr_with_every_check_passing_is_passing() {
        assert_eq!(
            merge_state("MERGEABLE", "CLEAN", Some("SUCCESS")),
            MergeStatus::Passing
        );
    }

    #[test]
    fn conflicting_branches_report_the_conflict() {
        assert_eq!(
            merge_state("CONFLICTING", "DIRTY", Some("SUCCESS")),
            MergeStatus::Conflicts,
            "a PR that cannot merge at all is not waiting on its checks"
        );
    }

    /// GitHub works mergeability out lazily, so a PR it has not looked at yet
    /// has no answer to give. Guessing one would be worse than saying so.
    #[test]
    fn uncomputed_mergeability_is_unknown() {
        assert_eq!(
            merge_state("UNKNOWN", "UNKNOWN", Some("SUCCESS")),
            MergeStatus::Unknown
        );
    }

    #[test]
    fn a_draft_is_a_draft_however_green_it_is() {
        let fields = format!(
            r#""isDraft":true,"mergeable":"MERGEABLE","mergeStateStatus":"CLEAN",
               "statusCheckRollup":{{"state":"SUCCESS"}},{NO_REVIEWS},{},{}"#,
            comments(&[]),
            threads(&[])
        );
        assert_eq!(
            merge_status(&pull_request_node(&fields)),
            MergeStatus::Draft
        );
    }

    #[test]
    fn a_failing_check_that_blocks_the_merge_is_failing() {
        assert_eq!(
            merge_state("MERGEABLE", "BLOCKED", Some("FAILURE")),
            MergeStatus::Failing
        );
        assert_eq!(
            merge_state("MERGEABLE", "BLOCKED", Some("ERROR")),
            MergeStatus::Failing
        );
    }

    /// `UNSTABLE` is GitHub's word for a PR that can merge even though a
    /// check is unhappy, which is to say the failing check is not one the
    /// base branch requires.
    #[test]
    fn a_failing_check_the_branch_does_not_require_still_merges() {
        assert_eq!(
            merge_state("MERGEABLE", "UNSTABLE", Some("FAILURE")),
            MergeStatus::OptionalFailing
        );
    }

    /// `BLOCKED` covers a missing approval just as readily as a failing
    /// check, so it must not be read as a verdict on the checks: this PR's
    /// checks have all passed and only its review is outstanding, which the
    /// Reviews column is the one to report.
    #[test]
    fn a_pr_blocked_only_on_review_still_reports_passing_checks() {
        assert_eq!(
            merge_state("MERGEABLE", "BLOCKED", Some("SUCCESS")),
            MergeStatus::Passing
        );
    }

    #[test]
    fn unfinished_checks_are_running() {
        assert_eq!(
            merge_state("MERGEABLE", "BLOCKED", Some("PENDING")),
            MergeStatus::Running
        );
        assert_eq!(
            merge_state("MERGEABLE", "BLOCKED", Some("EXPECTED")),
            MergeStatus::Running
        );
    }

    /// A repository with no CI has nothing to report, which is not the same
    /// as having something to report and it being bad.
    #[test]
    fn a_pr_with_no_checks_has_none_to_wait_for() {
        assert_eq!(
            merge_state("MERGEABLE", "CLEAN", None),
            MergeStatus::NoChecks
        );
    }

    /// Being behind the base branch only blocks the merge when the base
    /// branch requires it, which the query cannot see, so it is not reported
    /// as a blocker.
    #[test]
    fn being_behind_the_base_branch_reports_the_checks() {
        assert_eq!(
            merge_state("MERGEABLE", "BEHIND", Some("SUCCESS")),
            MergeStatus::Passing
        );
    }

    /// A pull request with nothing to say beyond its number.
    fn pull_request(number: u64) -> PullRequest {
        PullRequest {
            number,
            merge_status: MergeStatus::Passing.label().to_string(),
            review_status: "Pending".to_string(),
            comment_status: CommentStatus::Quiet.icon().to_string(),
            description: format!("pull request {number}"),
        }
    }

    /// The pull request numbers each group ended up with, in order.
    fn grouped(open: &[u64], stacks: &[Vec<u64>]) -> Vec<Vec<u64>> {
        let pull_requests = open.iter().copied().map(pull_request).collect();
        group_by_stack(pull_requests, stacks)
            .iter()
            .map(|group| group.pull_requests.iter().map(|pr| pr.number).collect())
            .collect()
    }

    /// The order GitHub returned is replaced by the order of the local
    /// changes, which is the one that says how the pull requests depend on
    /// each other.
    #[test]
    fn pull_requests_are_listed_in_local_stack_order() {
        assert_eq!(grouped(&[1, 2, 3], &[vec![3, 2, 1]]), vec![vec![3, 2, 1]]);
    }

    /// Each local stack stays a block of its own, in the order the local
    /// changes gave them.
    #[test]
    fn separate_stacks_stay_separate() {
        assert_eq!(
            grouped(&[1, 2, 3, 4], &[vec![3, 1], vec![4, 2]]),
            vec![vec![3, 1], vec![4, 2]]
        );
    }

    /// A pull request with no local change — landed elsewhere, or opened from
    /// another machine — is still open, so it is still listed. It just has no
    /// place among the stacks, and goes last.
    #[test]
    fn pull_requests_without_a_local_change_come_last() {
        assert_eq!(
            grouped(&[9, 1, 8, 2], &[vec![2, 1]]),
            vec![vec![2, 1], vec![9, 8]],
            "the pull requests with no local change keep GitHub's order"
        );
    }

    /// The stacks describe the local changes, which may name pull requests
    /// that are closed and so absent from the listing.
    #[test]
    fn stacks_may_name_pull_requests_that_are_not_listed() {
        assert_eq!(
            grouped(&[1], &[vec![7, 1], vec![8]]),
            vec![vec![1]],
            "a stack with nothing left to show is dropped rather than emptied"
        );
    }

    /// Only the stack column can distinguish the groups, so it appears exactly
    /// when there is more than one.
    #[test]
    fn the_stack_column_appears_only_when_it_separates_groups() {
        let one_stack = group_by_stack(vec![pull_request(2), pull_request(1)], &[vec![2, 1]]);
        let table = build_table(one_stack).to_string();
        assert!(
            !table.contains("Stack"),
            "one stack needs no Stack column, got:\n{table}"
        );

        let two_stacks =
            group_by_stack(vec![pull_request(2), pull_request(1)], &[vec![2], vec![1]]);
        let table = build_table(two_stacks).to_string();
        assert!(
            table.contains("Stack"),
            "two stacks need a Stack column, got:\n{table}"
        );
    }

    /// The markers have to say where each stack starts and ends, or the reader
    /// cannot tell one block from the next.
    #[test]
    fn stack_markers_bracket_each_stack() {
        let markers = |count: usize| {
            (0..count)
                .map(|position| stack_marker(true, position, count - 1))
                .map(|marker| console::strip_ansi_codes(&marker).into_owned())
                .collect::<Vec<_>>()
        };

        assert_eq!(markers(1), ["○"], "a stack of one is both ends at once");
        assert_eq!(markers(2), ["○", "╯"]);
        assert_eq!(markers(3), ["○", "│", "╯"]);
    }

    /// The pull requests no local change accounts for are not a stack, so
    /// there is nothing to draw beside them.
    #[test]
    fn loose_pull_requests_have_no_marker() {
        assert_eq!(stack_marker(false, 0, 1), "");
    }
}
