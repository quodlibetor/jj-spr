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

pub async fn list(graphql_client: reqwest::Client, config: &crate::config::Config) -> Result<()> {
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

    print_pr_info(response_body).context("Printing PR info".to_string())
}

#[derive(Tabled)]
struct Row {
    #[tabled(rename = "Reviews")]
    review_status: String,
    #[tabled(rename = "Comments")]
    comment_status: String,
    #[tabled(rename = "Description")]
    description: String,
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

fn print_pr_info(response_body: Response<search_query::ResponseData>) -> Result<()> {
    let rows = collect_rows(response_body);

    if rows.is_empty() {
        return Ok(());
    }

    let mut table = Table::new(rows);
    table.with(Style::sharp());

    let term = console::Term::stdout();
    term.write_line(&table.to_string())?;

    Ok(())
}

fn collect_rows(response_body: Response<search_query::ResponseData>) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();

    // A response without data, or without search nodes, means there is
    // simply nothing to list.
    let Some(data) = response_body.data else {
        return rows;
    };
    let Some(search_nodes) = data.search.nodes else {
        return rows;
    };

    for pr in search_nodes.into_iter().flatten() {
        let pr = match pr {
            crate::commands::list::search_query::SearchQuerySearchNodes::PullRequest(pr) => pr,
            _ => continue,
        };

        let comment_status = comment_status(&pr).icon().to_string();
        let review_status = review_status(&pr);

        let description = format!(
            "{}\n{}",
            console::style(&pr.title).bold(),
            console::style(&pr.url).dim(),
        );

        rows.push(Row {
            review_status,
            comment_status,
            description,
        });
    }

    rows
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
        let rows = collect_rows(response(pr_fields));
        assert_eq!(rows.len(), 1, "expected exactly one row");
        rows.into_iter().next().unwrap().comment_status
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
            r#""reviewDecision":{decision},"reviews":{{"nodes":[{states}]}},{},{}"#,
            comments(&[]),
            threads(&[])
        );
        let rows = collect_rows(response(&fields));
        assert_eq!(rows.len(), 1, "expected exactly one row");
        let cell = rows.into_iter().next().unwrap().review_status;
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
}
