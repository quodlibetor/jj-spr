/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use crate::error::{Error, Result, ResultExt};
use crate::output::output;
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

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "src/gql/schema.docs.graphql",
    query_path = "src/gql/pull_request_details.graphql",
    response_derives = "Debug"
)]
pub struct PullRequestDetails;

#[derive(Debug, clap::Parser)]
pub struct ListOptions {
    /// How to print the listing. Defaults to the `spr.listFormat` setting,
    /// and to `table` when that is not set either.
    #[clap(long, value_enum)]
    format: Option<ListFormat>,

    /// Also put the listing on the clipboard, as rich text where the format
    /// has links to carry: pasting it into a chat message gives real links
    /// rather than the text of a terminal escape.
    #[clap(long)]
    copy: bool,
}

/// The shapes a listing can be printed in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum ListFormat {
    /// A table of columns, one row per pull request.
    Table,
    /// A Markdown list to paste into a chat message asking for reviews: one
    /// bullet per pull request, its title under an emoji for where the review
    /// stands, and its URL on the line below.
    Slack,
    /// The same list, but with each title made a terminal hyperlink to its
    /// pull request instead of the URL being printed on its own line.
    SlackLinks,
}

impl ListFormat {
    /// Read the format from the `spr.listFormat` setting, which is spelled
    /// the way the command line spells it.
    fn from_config(git_config: &git2::Config) -> Result<Option<Self>> {
        let Some(value) = crate::config::get_config_value("spr.listFormat", git_config) else {
            return Ok(None);
        };

        <Self as clap::ValueEnum>::from_str(&value, true)
            .map(Some)
            .map_err(|message| Error::new(format!("spr.listFormat: {message}")))
    }
}

pub async fn list(
    opts: ListOptions,
    graphql_client: reqwest::Client,
    jj: &crate::jj::Jujutsu,
    config: &crate::config::Config,
) -> Result<()> {
    let format = match opts.format {
        Some(format) => format,
        None => ListFormat::from_config(&jj.git_repo.config()?)?.unwrap_or(ListFormat::Table),
    };

    let variables = search_query::Variables {
        query: format!(
            "repo:{}/{} is:open is:pr author:@me archived:false",
            config.owner, config.repo
        ),
    };

    // GitHub returns the pull requests in an order of its own, which says
    // nothing about how they depend on one another. GitHub's stacks and the
    // local changes do know, so they decide the order the table is printed in.
    let local_stacks = jj
        .get_local_pull_request_stacks(config)
        .context("Reading the local change stacks".to_string())?;

    // The chat formats print none of the table's own columns, so they do not
    // wait for GitHub to work them out. The table asks about the local
    // changes' pull requests alongside the search rather than after it, and
    // afterwards only about the ones the search found that no local change
    // carries.
    let table = format == ListFormat::Table;
    let local_numbers: Vec<u64> = if table {
        local_stacks.iter().flatten().copied().collect()
    } else {
        Vec::new()
    };
    let (response_body, columns) = tokio::join!(
        post::<SearchQuery>(&graphql_client, variables),
        fetch_table_columns(&graphql_client, config, &local_numbers),
    );
    let mut pull_requests = collect_pull_requests(response_body?);
    let mut columns = columns?;

    if table {
        let missing: Vec<u64> = pull_requests
            .iter()
            .map(|pull_request| pull_request.number)
            .filter(|number| !columns.contains_key(number))
            .collect();
        columns.extend(fetch_table_columns(&graphql_client, config, &missing).await?);

        for pull_request in &mut pull_requests {
            pull_request.table_columns = columns.remove(&pull_request.number);
        }
    }

    let stacks: Vec<Vec<u64>> = github_stacks(&pull_requests)
        .into_iter()
        .chain(local_stacks)
        .collect();

    print_pr_info(pull_requests, &stacks, format, opts.copy).context("Printing PR info".to_string())
}

/// Send one GraphQL query.
async fn post<Q: GraphQLQuery>(
    graphql_client: &reqwest::Client,
    variables: Q::Variables,
) -> Result<Response<Q::ResponseData>> {
    let response = graphql_client
        .post("https://api.github.com/graphql")
        .json(&Q::build_query(variables))
        .send()
        .await?;

    Ok(response.json().await?)
}

/// The columns only the table prints, for each of the pull requests
/// `numbers`, asked for one pull request per request with the requests sent
/// together. A number GitHub has no pull request for is left out.
async fn fetch_table_columns(
    graphql_client: &reqwest::Client,
    config: &crate::config::Config,
    numbers: &[u64],
) -> Result<std::collections::HashMap<u64, TableColumns>> {
    let responses = futures::future::try_join_all(numbers.iter().map(|&number| async move {
        let variables = pull_request_details::Variables {
            owner: config.owner.clone(),
            name: config.repo.clone(),
            number: number as i64,
        };
        Ok::<_, Error>((
            number,
            post::<PullRequestDetails>(graphql_client, variables).await?,
        ))
    }))
    .await?;

    Ok(responses
        .into_iter()
        .filter_map(|(number, response)| {
            let details = response.data?.repository?.pull_request?;
            Some((
                number,
                TableColumns {
                    merge_status: merge_status(&details).label().to_string(),
                    comment_status: comment_status(&details).icon().to_string(),
                },
            ))
        })
        .collect())
}

/// The GitHub stacks `pull_requests` are in, each ordered the way the listing
/// prints a stack: top first, the way `jj log` reads.
///
/// Only the pull requests being listed are placed, so a member that is merged,
/// closed or someone else's leaves no gap. The stacks themselves are put newest
/// first, by stack number.
fn github_stacks(pull_requests: &[PullRequest]) -> Vec<Vec<u64>> {
    let mut stacks: std::collections::BTreeMap<std::cmp::Reverse<u64>, Vec<(u64, u64)>> =
        std::collections::BTreeMap::new();
    for pull_request in pull_requests {
        if let Some(entry) = pull_request.stack_entry {
            stacks
                .entry(std::cmp::Reverse(entry.stack_number))
                .or_default()
                .push((entry.position, pull_request.number));
        }
    }

    stacks
        .into_values()
        .map(|mut members| {
            members.sort_by_key(|&(position, _)| std::cmp::Reverse(position));
            members.into_iter().map(|(_, number)| number).collect()
        })
        .collect()
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
fn merge_status(pr: &pull_request_details::PullRequestDetailsRepositoryPullRequest) -> MergeStatus {
    use pull_request_details::{MergeStateStatus, MergeableState, StatusState};

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
fn review_status(pr: &search_query::SearchQuerySearchNodesOnPullRequest) -> ReviewStatus {
    match pr.review_decision {
        Some(search_query::PullRequestReviewDecision::APPROVED) => ReviewStatus::Accepted,
        Some(search_query::PullRequestReviewDecision::CHANGES_REQUESTED) => {
            ReviewStatus::ChangesRequested
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
                ReviewStatus::Commented
            } else {
                ReviewStatus::Pending
            }
        }
        Some(search_query::PullRequestReviewDecision::Other(ref d)) => {
            ReviewStatus::Other(d.clone())
        }
    }
}

/// Where a pull request stands with its reviewers.
#[derive(Debug, PartialEq, Eq)]
enum ReviewStatus {
    /// A reviewer approved it.
    Accepted,
    /// A reviewer asked for changes.
    ChangesRequested,
    /// A reviewer said something without reaching a verdict.
    Commented,
    /// Nobody has reviewed it yet.
    Pending,
    /// A verdict this build does not know, reported as GitHub words it.
    Other(String),
}

impl ReviewStatus {
    /// How the status reads in the table.
    fn label(&self) -> String {
        match self {
            ReviewStatus::Accepted => console::style("Accepted").green().to_string(),
            ReviewStatus::ChangesRequested => console::style("Changes Requested").red().to_string(),
            ReviewStatus::Commented => console::style("Commented").yellow().to_string(),
            ReviewStatus::Pending => "Pending".to_string(),
            ReviewStatus::Other(decision) => decision.clone(),
        }
    }

    /// How the status reads in a chat message, where there is no column to
    /// put a word in and colour does not survive the paste.
    fn emoji(&self) -> &'static str {
        match self {
            ReviewStatus::Accepted => "✅",
            ReviewStatus::ChangesRequested => "🔴",
            ReviewStatus::Commented => "💬",
            ReviewStatus::Pending => "⏳",
            ReviewStatus::Other(_) => "❔",
        }
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
fn comment_status(
    pr: &pull_request_details::PullRequestDetailsRepositoryPullRequest,
) -> CommentStatus {
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
    review_status: ReviewStatus,
    title: String,
    url: String,
    stack_entry: Option<StackEntry>,
    /// Fetched only for the table, and missing where GitHub did not answer
    /// for this pull request.
    table_columns: Option<TableColumns>,
}

/// The columns only the table prints.
struct TableColumns {
    merge_status: String,
    comment_status: String,
}

/// Where a pull request sits in a GitHub stack.
#[derive(Clone, Copy)]
struct StackEntry {
    stack_number: u64,
    /// 1 is the pull request nearest the base branch.
    position: u64,
}

/// One block of the listing.
struct Group {
    pull_requests: Vec<PullRequest>,
    /// Whether this group is a stack. A pull request that no stack accounts
    /// for is a group of its own, which has nothing to draw in the Stack
    /// column.
    is_stack: bool,
}

fn print_pr_info(
    pull_requests: Vec<PullRequest>,
    stacks: &[Vec<u64>],
    format: ListFormat,
    copy: bool,
) -> Result<()> {
    let groups = group_by_stack(pull_requests, stacks);

    if groups.is_empty() {
        return Ok(());
    }

    // The clipboard flavours are built before the listing is consumed, since
    // building either one takes the pull requests.
    let clipboard = copy.then(|| clipboard_flavours(&groups, format));

    let text = match format {
        ListFormat::Table => build_table(&groups),
        ListFormat::Slack => build_list(&groups, Link::OwnLine),
        ListFormat::SlackLinks => build_list(&groups, Link::OnTheTitle),
    };

    let term = console::Term::stdout();
    term.write_line(&text)?;

    if let Some(flavours) = clipboard {
        put_on_clipboard(flavours).context("Copying the listing to the clipboard".to_string())?;
        output("📋", "Copied to the clipboard")?;
    }

    Ok(())
}

/// What `--copy` puts on the clipboard.
struct Flavours {
    /// The plain text an application that wants no formatting will take.
    text: String,
    /// The HTML flavour, where the format has links to carry. Chat clients
    /// take this one, which is how a paste ends up with real links rather
    /// than with the URLs written out.
    html: Option<String>,
}

/// Build what `--copy` puts on the clipboard for `format`.
///
/// The chat formats agree once they are HTML — a title that is a link is what
/// both of them were reaching for — so the two differ only in the plain text
/// they fall back to. That plain text is the spelled-out form for both, since
/// the terminal escapes that make a hyperlink on screen paste as rubbish
/// everywhere else.
fn clipboard_flavours(groups: &[Group], format: ListFormat) -> Flavours {
    match format {
        ListFormat::Table => Flavours {
            text: build_table(groups),
            html: None,
        },
        ListFormat::Slack | ListFormat::SlackLinks => Flavours {
            text: build_list(groups, Link::OwnLine),
            html: Some(build_html_list(groups)),
        },
    }
}

fn put_on_clipboard(flavours: Flavours) -> Result<()> {
    let mut clipboard = arboard::Clipboard::new()?;

    match flavours.html {
        Some(html) => clipboard.set().html(html, Some(flavours.text))?,
        None => clipboard.set_text(flavours.text)?,
    }

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

        let review_status = review_status(&pr);

        pull_requests.push(PullRequest {
            number: pr.number as u64,
            review_status,
            title: pr.title,
            url: pr.url,
            stack_entry: pr.stack_entry.and_then(|entry| {
                Some(StackEntry {
                    stack_number: entry.stack?.number as u64,
                    position: entry.position as u64,
                })
            }),
            table_columns: None,
        });
    }

    pull_requests
}

/// Sort the pull requests into `stacks`: GitHub's stacks followed by the ones
/// the local changes describe.
///
/// The stacks are listed in the order they were given, each in its own group,
/// and a stack claims only the pull requests no earlier stack has, so where
/// GitHub and the local changes disagree, GitHub's stack wins. A pull request
/// that no stack carries — one landed elsewhere, or opened from another
/// machine — is listed after the stacks, each in a group of its own, rather
/// than being dropped from a listing that is meant to show everything open.
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

    groups.extend(unclaimed.into_iter().flatten().map(|pull_request| Group {
        pull_requests: vec![pull_request],
        is_stack: false,
    }));

    groups
}

/// Build the listing as tables, one per group, separated by a blank line.
fn build_table(groups: &[Group]) -> String {
    // The Stack column is only worth its width when there is more than one
    // group: a listing that is one stack from top to bottom already reads in
    // order without it.
    let stack_column_earns_its_place = groups.len() > 1;

    groups
        .iter()
        .map(|group| {
            let last = group.pull_requests.len() - 1;
            let rows = group
                .pull_requests
                .iter()
                .enumerate()
                .map(|(position, pull_request)| Row {
                    stack: stack_marker(group.is_stack, position, last),
                    merge_status: pull_request.table_columns.as_ref().map_or_else(
                        || MergeStatus::Unknown.label().to_string(),
                        |columns| columns.merge_status.clone(),
                    ),
                    review_status: pull_request.review_status.label(),
                    comment_status: pull_request
                        .table_columns
                        .as_ref()
                        .map_or_else(String::new, |columns| columns.comment_status.clone()),
                    description: format!(
                        "{}\n{}",
                        console::style(&pull_request.title).bold(),
                        console::style(&pull_request.url).dim(),
                    ),
                });

            let mut builder = Table::builder(rows);
            if !stack_column_earns_its_place {
                builder.remove_column(0);
            }

            let mut table = builder.build();
            table.with(Style::sharp());

            table.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Where the URL of a pull request goes in a chat listing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Link {
    /// Printed under the title, on a line of its own. Chat clients make a
    /// bare URL a link, so this pastes as a link wherever it lands.
    OwnLine,
    /// Attached to the title as a terminal hyperlink, so the bullet is one
    /// short line. Terminals that do not know the escape show the title and
    /// nothing else, and so, at the time of writing, does a paste into Slack.
    OnTheTitle,
}

/// Build the listing as a Markdown bullet list to paste into a chat message
/// asking for reviews.
///
/// Each stack is a block of its own, separated by a blank line, in the order
/// the table would have printed them: newest change first, the way `jj log`
/// reads. There are no stack markers, because indentation is all a chat
/// client will keep, and no merge or comment status, because what a reviewer
/// needs from the message is which pull requests are still waiting on them.
fn build_list(groups: &[Group], link: Link) -> String {
    groups
        .iter()
        .map(|group| {
            group
                .pull_requests
                .iter()
                .map(|pull_request| bullet(pull_request, link))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Build the listing as HTML, one list per stack, each title a link to its
/// pull request. This is the flavour a chat client pastes from.
fn build_html_list(groups: &[Group]) -> String {
    groups
        .iter()
        .map(|group| {
            let items: String = group
                .pull_requests
                .iter()
                .map(|pull_request| {
                    format!(
                        "<li>{} <a href=\"{}\">{}</a></li>",
                        pull_request.review_status.emoji(),
                        escape_html(&pull_request.url),
                        escape_html(&pull_request.title),
                    )
                })
                .collect();
            format!("<ul>{items}</ul>")
        })
        .collect()
}

/// Escape the characters that would otherwise be read as markup. A pull
/// request title is somebody's prose and a URL carries query strings, so both
/// can hold any of them.
fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn bullet(pull_request: &PullRequest, link: Link) -> String {
    let emoji = pull_request.review_status.emoji();
    let title = &pull_request.title;
    let url = &pull_request.url;

    match link {
        Link::OwnLine => format!("- {emoji} {title}\n  {url}"),
        Link::OnTheTitle => format!("- {emoji} {}", hyperlink(url, title)),
    }
}

/// Wrap `text` in the OSC 8 escape sequence that makes a terminal draw it as
/// a link to `url`.
///
/// The sequence is `ESC ] 8 ; ; URL ST`, the text, then the same with an
/// empty URL to close it. `ST` is written as `ESC \\`, which every terminal
/// that understands the escape accepts, rather than the BEL some also take.
fn hyperlink(url: &str, text: &str) -> String {
    format!("\u{1b}]8;;{url}\u{1b}\\{text}\u{1b}]8;;\u{1b}\\")
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

    /// The table's own columns for one pull request, from a payload built the
    /// way [`response`] builds one.
    fn pull_request_node(
        pr_fields: &str,
    ) -> pull_request_details::PullRequestDetailsRepositoryPullRequest {
        let json = format!(r#"{{"data":{{"repository":{{"pullRequest":{{{pr_fields}}}}}}}}}"#);
        let response: Response<pull_request_details::ResponseData> = serde_json::from_str(&json)
            .expect("test payload should match the query's response shape");
        response
            .data
            .expect("the payload should have data")
            .repository
            .expect("the payload should have a repository")
            .pull_request
            .expect("the payload should describe a pull request")
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
        let pr = pull_request_node(&format!("{},{pr_fields}", ready_to_merge()));
        comment_status(&pr).icon().to_string()
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

    /// Where a pull request stands with its reviewers, as the payload says.
    fn review_state(decision: &str, review_states: &[&str]) -> ReviewStatus {
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
        pull_requests.into_iter().next().unwrap().review_status
    }

    /// The Reviews cell, stripped of styling so tests assert on wording.
    fn review_cell(decision: &str, review_states: &[&str]) -> String {
        let label = review_state(decision, review_states).label();
        console::strip_ansi_codes(&label).into_owned()
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
            review_status: ReviewStatus::Pending,
            title: format!("pull request {number}"),
            url: format!("https://github.com/o/r/pull/{number}"),
            stack_entry: None,
            table_columns: Some(TableColumns {
                merge_status: MergeStatus::Passing.label().to_string(),
                comment_status: CommentStatus::Quiet.icon().to_string(),
            }),
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
    /// place among the stacks, so it goes last, apart from everything else.
    #[test]
    fn pull_requests_without_a_local_change_come_last_each_on_its_own() {
        assert_eq!(
            grouped(&[9, 1, 8, 2], &[vec![2, 1]]),
            vec![vec![2, 1], vec![9], vec![8]],
            "the pull requests with no local change keep GitHub's order"
        );
    }

    /// GitHub's stacks come before the local ones, so a pull request both of
    /// them name is listed where GitHub's stack puts it, and the local stack
    /// keeps only what is left.
    #[test]
    fn a_github_stack_claims_its_pull_requests_before_a_local_one() {
        let github = vec![vec![3, 2]];
        let local = vec![vec![4, 3, 2, 1]];
        let stacks: Vec<Vec<u64>> = github.into_iter().chain(local).collect();
        assert_eq!(
            grouped(&[1, 2, 3, 4], &stacks),
            vec![vec![3, 2], vec![4, 1]]
        );
    }

    fn stacked(number: u64, stack_number: u64, position: u64) -> PullRequest {
        PullRequest {
            stack_entry: Some(StackEntry {
                stack_number,
                position,
            }),
            ..pull_request(number)
        }
    }

    /// GitHub numbers a stack's positions from the bottom, and the listing
    /// prints the top first. A member that is not listed — merged, closed, or
    /// someone else's — is simply absent.
    #[test]
    fn github_stacks_are_read_top_first_and_newest_first() {
        let pull_requests = vec![
            stacked(2, 10, 2),
            pull_request(9),
            stacked(5, 20, 2),
            stacked(3, 10, 3),
            stacked(4, 20, 1),
        ];
        assert_eq!(github_stacks(&pull_requests), vec![vec![5, 4], vec![3, 2]]);
    }

    /// Only the stack column can distinguish the groups, so it appears exactly
    /// when there is more than one.
    #[test]
    fn the_stack_column_appears_only_when_it_separates_groups() {
        let one_stack = group_by_stack(vec![pull_request(2), pull_request(1)], &[vec![2, 1]]);
        let table = build_table(&one_stack);
        assert!(
            !table.contains("Stack"),
            "one stack needs no Stack column, got:\n{table}"
        );

        let two_stacks =
            group_by_stack(vec![pull_request(2), pull_request(1)], &[vec![2], vec![1]]);
        let table = build_table(&two_stacks);
        assert!(
            table.contains("Stack"),
            "two stacks need a Stack column, got:\n{table}"
        );
    }

    /// Each group is a table of its own, with a blank line between them.
    #[test]
    fn each_group_is_its_own_table() {
        let groups = group_by_stack(
            vec![pull_request(3), pull_request(2), pull_request(1)],
            &[vec![3, 2]],
        );
        let table = build_table(&groups);
        let blocks: Vec<&str> = table.split("\n\n").collect();
        assert_eq!(blocks.len(), 2, "got:\n{table}");
        assert!(blocks[0].contains("pull request 3") && blocks[0].contains("pull request 2"));
        assert!(blocks[1].contains("pull request 1"));
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

    /// The chat listing is what gets pasted into a review request, so each
    /// bullet has to carry the URL it is asking someone to open.
    #[test]
    fn the_chat_list_prints_a_bullet_and_a_url_per_pull_request() {
        let groups = group_by_stack(vec![pull_request(2), pull_request(1)], &[vec![2, 1]]);
        assert_eq!(
            build_list(&groups, Link::OwnLine),
            "- \u{23f3} pull request 2\n  https://github.com/o/r/pull/2\n\
             - \u{23f3} pull request 1\n  https://github.com/o/r/pull/1"
        );
    }

    /// A blank line is the only thing that tells one stack from the next once
    /// the Stack column is gone.
    #[test]
    fn separate_stacks_are_separate_blocks_in_the_chat_list() {
        let groups = group_by_stack(vec![pull_request(2), pull_request(1)], &[vec![2], vec![1]]);
        let list = build_list(&groups, Link::OwnLine);
        assert_eq!(
            list.split("\n\n").count(),
            2,
            "the two stacks should be two blocks, got:\n{list}"
        );
    }

    /// The hyperlink form exists to keep a bullet to one line, so the URL
    /// must be in the escape and nowhere else.
    #[test]
    fn a_hyperlinked_title_carries_the_url_on_one_line() {
        let bullet = bullet(&pull_request(1), Link::OnTheTitle);
        assert_eq!(
            bullet,
            "- \u{23f3} \u{1b}]8;;https://github.com/o/r/pull/1\u{1b}\\\
             pull request 1\u{1b}]8;;\u{1b}\\"
        );
        assert!(!bullet.contains('\n'), "got:\n{bullet}");
    }

    /// The emoji is the whole report in a chat message, so two statuses that
    /// share one would leave the reader unable to tell them apart.
    #[test]
    fn each_review_status_has_an_emoji_of_its_own() {
        let statuses = [
            ReviewStatus::Accepted,
            ReviewStatus::ChangesRequested,
            ReviewStatus::Commented,
            ReviewStatus::Pending,
            ReviewStatus::Other("SOMETHING_NEW".to_string()),
        ];
        let emojis: std::collections::HashSet<_> =
            statuses.iter().map(ReviewStatus::emoji).collect();
        assert_eq!(emojis.len(), statuses.len(), "got {emojis:?}");
    }

    /// The emoji has to follow GitHub's verdict, not just exist.
    #[test]
    fn the_emoji_reports_the_review_decision() {
        assert_eq!(
            review_state(r#""APPROVED""#, &["APPROVED"]).emoji(),
            ReviewStatus::Accepted.emoji()
        );
        assert_eq!(
            review_state(r#""CHANGES_REQUESTED""#, &["CHANGES_REQUESTED"]).emoji(),
            ReviewStatus::ChangesRequested.emoji()
        );
        assert_eq!(
            review_state("null", &[]).emoji(),
            ReviewStatus::Pending.emoji()
        );
    }

    /// `spr.listFormat` is read through the same parser as the command line,
    /// so the setting is spelled the way the flag is.
    #[test]
    fn the_setting_is_spelled_the_way_the_flag_is() {
        use clap::ValueEnum;

        assert_eq!(ListFormat::from_str("table", true), Ok(ListFormat::Table));
        assert_eq!(ListFormat::from_str("slack", true), Ok(ListFormat::Slack));
        assert_eq!(
            ListFormat::from_str("slack-links", true),
            Ok(ListFormat::SlackLinks)
        );
        assert!(ListFormat::from_str("not-a-format", true).is_err());
    }

    /// The HTML flavour is what makes a paste into a chat message a set of
    /// real links, so every title has to be an anchor to its pull request.
    #[test]
    fn the_html_flavour_makes_every_title_a_link() {
        let groups = group_by_stack(vec![pull_request(2), pull_request(1)], &[vec![2, 1]]);
        assert_eq!(
            build_html_list(&groups),
            "<ul>\
             <li>\u{23f3} <a href=\"https://github.com/o/r/pull/2\">pull request 2</a></li>\
             <li>\u{23f3} <a href=\"https://github.com/o/r/pull/1\">pull request 1</a></li>\
             </ul>"
        );
    }

    /// Each stack is a list of its own, which is what keeps the blocks apart
    /// once the blank lines of the plain text are gone.
    #[test]
    fn each_stack_is_its_own_html_list() {
        let groups = group_by_stack(vec![pull_request(2), pull_request(1)], &[vec![2], vec![1]]);
        assert_eq!(build_html_list(&groups).matches("<ul>").count(), 2);
    }

    /// A title is somebody's prose and a URL carries query strings, so either
    /// can hold characters that would otherwise close the markup around them.
    #[test]
    fn markup_in_a_title_or_url_is_escaped() {
        let pull_request = PullRequest {
            title: r#"fix: <script> & "quotes""#.to_string(),
            url: "https://github.com/o/r/pull/1?a=1&b=2".to_string(),
            ..pull_request(1)
        };
        let groups = vec![Group {
            pull_requests: vec![pull_request],
            is_stack: true,
        }];

        assert_eq!(
            build_html_list(&groups),
            "<ul><li>\u{23f3} <a href=\"https://github.com/o/r/pull/1?a=1&amp;b=2\">\
             fix: &lt;script&gt; &amp; &quot;quotes&quot;</a></li></ul>"
        );
    }

    /// The escapes that draw a hyperlink on screen paste as rubbish, so the
    /// plain-text flavour spells the URLs out whichever chat format was asked
    /// for. The HTML flavour is the same either way: a title that is a link is
    /// what both formats were reaching for.
    #[test]
    fn the_clipboard_never_carries_terminal_escapes() {
        let groups = group_by_stack(vec![pull_request(1)], &[vec![1]]);

        for format in [ListFormat::Slack, ListFormat::SlackLinks] {
            let flavours = clipboard_flavours(&groups, format);
            assert!(
                !flavours.text.contains('\u{1b}'),
                "{format:?} put escapes on the clipboard: {:?}",
                flavours.text
            );
            assert_eq!(flavours.text, build_list(&groups, Link::OwnLine));
            assert_eq!(
                flavours.html.as_deref(),
                Some(build_html_list(&groups)).as_deref()
            );
        }
    }

    /// A table has no links to carry, so there is nothing for an HTML flavour
    /// to add over the text of the table itself.
    #[test]
    fn copying_a_table_copies_the_table() {
        let groups = group_by_stack(vec![pull_request(1)], &[vec![1]]);
        let flavours = clipboard_flavours(&groups, ListFormat::Table);
        assert_eq!(flavours.text, build_table(&groups));
        assert_eq!(flavours.html, None);
    }
}
