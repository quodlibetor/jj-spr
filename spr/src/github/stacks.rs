/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A client for GitHub's Stacked Pull Requests REST API: the stack resources it
//! answers with, the failures it answers with, and the [`GitHub`] methods that
//! call it.

use super::GitHub;

/// A stack of pull requests, as returned by GitHub's Stacked Pull Requests
/// REST API.
///
/// The shape is taken from responses captured off the live API on 2026-07-31
/// rather than from the published schema, which describes it only in part.
/// Fields GitHub sends that nothing here reads — a member's author, node ids,
/// the various URLs — are simply not modelled; serde ignores them.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Stack {
    /// GitHub's internal database identifier for the stack. This is *not* how
    /// the stack is addressed; see [`Stack::number`].
    pub id: u64,

    /// The stack number, which is what github.com shows and what the API paths
    /// take. Distinct from [`Stack::id`], and drawn from the same per-repository
    /// counter as pull request numbers — so a stack number and a pull request
    /// number are never interchangeable but do look alike.
    pub number: u64,

    /// The ref the bottom pull request of the stack is based on. Carries only a
    /// ref name; unlike a member's `head`/`base`, GitHub sends no SHA here.
    pub base: StackBase,

    /// Whether the stack is still open. It goes to `false` once the stack has
    /// been unstacked down to merged members only, and such stacks are still
    /// returned by the list endpoints — see [`GitHub::get_stacks`].
    pub open: bool,

    /// The stack's pull requests, ordered bottom to top.
    #[serde(default)]
    pub pull_requests: Vec<StackPullRequest>,

    /// When the stack was created, as GitHub's ISO 8601 timestamp.
    #[serde(default)]
    pub created_at: Option<String>,
}

impl Stack {
    /// The stack's pull request numbers, ordered bottom to top.
    pub fn pull_request_numbers(&self) -> Vec<u64> {
        self.pull_requests.iter().map(|pr| pr.number).collect()
    }
}

/// The base of a [`Stack`] as a whole.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct StackBase {
    #[serde(rename = "ref")]
    pub ref_name: String,
}

/// One of the pull requests in a [`Stack`].
///
/// Only `number`, `state` and `head` are required. Everything else is defaulted
/// on purpose, whether or not GitHub was observed to send it: none of it is
/// worth failing an entire response over, and a response is all-or-nothing —
/// one absent key costs the caller every stack in the repository.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct StackPullRequest {
    pub number: u64,
    pub state: StackPullRequestState,

    pub head: StackGitRef,

    /// The pull request's *live* base, so one that GitHub retargeted when the
    /// stack below it merged shows the new value rather than the old chain link.
    ///
    /// The one capture of the list response was trimmed before it was saved, so
    /// there is no evidence the list form carries this at all — read it from
    /// [`GitHub::get_stack`] rather than from a list if it matters.
    #[serde(default)]
    pub base: Option<StackGitRef>,

    #[serde(default)]
    pub draft: bool,

    #[serde(default)]
    pub merged_at: Option<String>,

    #[serde(default)]
    pub title: Option<String>,
}

impl StackPullRequest {
    /// Whether the pull request has been merged.
    ///
    /// [`StackPullRequestState`] has no merged state — a merged pull request is
    /// `Closed` — so this is the only way to tell a merged member from one that
    /// was closed unmerged. Merged members stay in the stack indefinitely, as
    /// history; they do not leave it and they do not stop the stack accepting
    /// more pull requests.
    pub fn is_merged(&self) -> bool {
        self.merged_at.is_some()
    }
}

/// The state of a pull request inside a [`Stack`].
///
/// Deliberately not [`PullRequestState`](super::PullRequestState): that enum is
/// what jj-spr *sends* in a pull request update, so it cannot grow a catch-all
/// without also being able to send it. Here the vocabulary is GitHub's, and a
/// value we do not recognise must not fail the whole response — one unexpected
/// string would otherwise cost the caller every stack in the repository, not
/// just this pull request.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StackPullRequestState {
    Open,
    Closed,

    #[serde(other)]
    Unknown,
}

/// A ref named on one of a stack's pull requests, exactly as the stacks API
/// sends it.
///
/// Not to be confused with [`GitHubBranch`](super::GitHubBranch), which is the
/// ref abstraction the rest of the crate uses: that one normalises `refs/heads/`
/// and knows the local and remote forms of a branch, whereas this is the raw
/// wire shape.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct StackGitRef {
    #[serde(rename = "ref")]
    pub ref_name: String,

    #[serde(default)]
    pub sha: Option<String>,
}

/// What became of a stack after asking GitHub to unstack it.
///
/// Unstacking releases every member it can and never rewrites any base ref. It
/// takes no arguments: there is no way to remove one pull request from a stack.
#[derive(Debug, Clone)]
pub enum UnstackOutcome {
    /// Every member was released and the stack record itself is gone: its number
    /// no longer resolves and it disappears from the list endpoint.
    Dissolved,

    /// The stack record survives, holding the members GitHub would not release.
    ///
    /// Do not read this as "retry": there are two quite different reasons a
    /// member stays, and the outcome alone does not say which. *Merged* pull
    /// requests are kept as immutable history — that case was observed, it
    /// leaves the stack `open: false`, and there is nothing to act on. GitHub's
    /// own client also reports members that are queued for merge or have
    /// auto-merge enabled being left behind, and those are still open and still
    /// hold the lock on their base ref. Tell them apart with [`Stack::open`] and
    /// [`StackPullRequest::is_merged`], not from the fact that a stack came back.
    Retained(Stack),
}

/// Where a request to GitHub's asynchronous merge has got to.
///
/// The endpoint answers with a `status` and a bag of `details`, and the same
/// request can be made again at any time: while a merge is in flight it is
/// refused as a duplicate, and once the pull request is merged it answers that
/// it is. So these three are the whole state of a merge as far as a caller can
/// see it — and only the last is an end state.
#[derive(Debug, PartialEq, Eq)]
pub enum AsyncMerge {
    /// GitHub has taken the request and will merge in its own time. Nothing has
    /// been merged yet.
    Enqueued,
    /// A merge request for this pull request was already in flight, so this one
    /// changed nothing. Whatever is in flight is still going to happen, which is
    /// why this is not an error: asking twice for the merge that is already
    /// coming is the same request, not a conflicting one.
    AlreadyEnqueued,
    /// The pull request is merged. `sha` is the commit it landed as, where
    /// GitHub named one.
    Merged { sha: Option<String> },
}

/// What GitHub answers a merge request with, on every status.
#[derive(Debug, serde::Deserialize)]
struct AsyncMergeResponse {
    status: String,
    #[serde(default)]
    details: AsyncMergeDetails,
}

/// The `details` of a merge request's answer.
///
/// Only the merge commit is modelled, and optionally, because which fields
/// GitHub sends depends on the status and none of them is worth failing a
/// response over. `message`, `uuid`, `merge_method`, `merge_action` and
/// `expected_head_sha` are all sent and all ignored: nothing acts on them, and
/// `expected_head_sha` in particular must not be mistaken for a lease — see
/// [`GitHub::merge_pull_request_async`].
#[derive(Debug, Default, serde::Deserialize)]
struct AsyncMergeDetails {
    #[serde(default)]
    sha: Option<String>,
}

/// The body a merge request takes.
#[derive(serde::Serialize)]
struct AsyncMergeRequest {
    merge_method: &'static str,
}

pub type StackResult<T> = std::result::Result<T, StackApiError>;

/// Classify a stacks endpoint's failures. Every stacks call goes through this,
/// which is the only thing standing between a caller and an unclassified error.
trait StackScopeResultExt<T> {
    fn on_stack_scope(self, scope: StackScope) -> StackResult<T>;
}

impl<T> StackScopeResultExt<T> for std::result::Result<T, octocrab::Error> {
    fn on_stack_scope(self, scope: StackScope) -> StackResult<T> {
        self.map_err(|error| stack_api_error(scope, error))
    }
}

/// A failure from one of the Stacked Pull Requests endpoints.
///
/// These endpoints answer with statuses and validation messages that callers
/// have to act on differently, but [`crate::error::Error`] flattens everything
/// to strings and so cannot carry an HTTP status. Rather than teach the
/// crate-wide error type about HTTP — which would perturb every existing `?` —
/// the stack methods keep status handling here and report this instead.
///
/// **Match on this before letting it out.** `crate::error::Error` has a blanket
/// `From` for anything implementing [`std::error::Error`], so `?`-ing a
/// [`StackResult`] into a `crate::Result` compiles happily and reduces the whole
/// enum to one string — which is exactly the classification this type exists to
/// preserve, discarded silently and un-greppably. Every other method on
/// [`GitHub`] returns `crate::Result`, so `?` is the reflex here.
#[derive(Debug, thiserror::Error)]
pub enum StackApiError {
    /// 404 from `/repos/{owner}/{repo}/stacks`: taken to mean the repository has
    /// not opted in to stacked pull requests, so callers degrade to jj-spr's own
    /// stacking rather than fail.
    ///
    /// It is an inference from the status, and two other things produce the same
    /// 404. GitHub answers 404 rather than 403 for a repository the token cannot
    /// see, so an expired, under-scoped or SSO-unauthorized token looks exactly
    /// like a repository without stacks — as does a misconfigured
    /// `spr.githubRepository`. And the inference needs GitHub's usual JSON error
    /// body: a 404 delivered as HTML or with no body, as a proxy or a GitHub
    /// Enterprise front end may do, arrives as [`StackApiError::Other`] and does
    /// not degrade at all. Anything that degrades on this should say so out loud
    /// rather than quietly do less than the user asked for.
    #[error("this repository does not have stacked pull requests enabled")]
    NotEnabled,

    /// 404 from a route addressing one stack: that stack does not exist any
    /// more. Merging does not do this — a landed pull request stays in its stack
    /// and the stack keeps answering — but unstacking a stack whose members are
    /// all unmerged destroys the record outright, and restructuring a stack means
    /// exactly that followed by a fresh one with a new number. So a stack number
    /// goes stale as easily as it goes wrong, and neither says anything about
    /// whether the repository supports stacks.
    #[error("stack #{stack_number} does not exist")]
    StackNotFound { stack_number: u64 },

    /// 422: fewer pull requests were offered than the endpoint's minimum, which
    /// is two to create a stack and one to add to it.
    #[error("too few pull requests: creating a stack takes two, adding to one takes one")]
    TooFewPullRequests,

    /// 422: the pull requests do not chain base-to-head.
    #[error(
        "the pull requests do not form a stack: each pull request's base ref must be the previous pull request's head ref"
    )]
    NotAChain,

    /// 404 from the asynchronous merge route: the pull request is not there, or
    /// the route is not.
    ///
    /// The two cannot be told apart, and both are dead ends for a caller, so
    /// they share a variant rather than pretending to a distinction GitHub does
    /// not make. Unlike [`StackApiError::NotEnabled`], nothing degrades on this:
    /// a caller that asked for the stack merge asked for this route.
    #[error("GitHub has no asynchronous merge for pull request #{number}")]
    MergeAsyncNotFound { number: u64 },

    /// 422: some of the pull requests already belong to a stack.
    ///
    /// `pull_requests` holds the numbers GitHub named, echoed back in the order
    /// they were *requested* rather than stack order. They are worth reporting
    /// but are a poor basis for a decision: the message cannot distinguish "these
    /// are already exactly the stack we wanted" from "these are scattered across
    /// other stacks", and it says nothing about the order they are stacked in —
    /// GitHub raises it before it ever checks the chain. To find out what really
    /// exists, ask [`GitHub::get_open_stack_for_pull_request`].
    #[error("some of these pull requests are already part of a stack")]
    AlreadyStacked { pull_requests: Vec<u64> },

    /// A refusal from GitHub that does not match anything above, keeping its
    /// status and message so a caller can report what actually happened.
    ///
    /// This variant is load-bearing rather than a fallback nobody hits: the
    /// validation messages are English prose with no version guarantee, and they
    /// have already changed once — everything the reference implementation
    /// matched on had been reworded by the time this was written. Recognising a
    /// message is an optimisation; preserving it is the contract.
    #[error("GitHub refused the request ({status}): {message}")]
    Rejected { status: u16, message: String },

    /// A failure that never reached GitHub's error body: a transport error, or a
    /// response whose body could not be parsed.
    ///
    /// Deliberately not `#[from]`. Without the conversion, a stacks method that
    /// tries to `?` an `octocrab` call straight into a [`StackResult`] does not
    /// compile, and has to go through `StackScopeResultExt::on_stack_scope`
    /// instead — which is the only thing that classifies the failure at all.
    #[error(transparent)]
    Other(octocrab::Error),
}

/// Which stacks route a request went to. A 404 means different things on each,
/// so classification cannot be done from the status alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StackScope {
    /// `/repos/{owner}/{repo}/stacks`, which exists whenever the repository has
    /// stacks enabled.
    Repository,

    /// A route addressing one stack by its number.
    Stack { stack_number: u64 },

    /// The asynchronous merge route of one pull request, which is a route on the
    /// pull request rather than on any stack — even though merging one member of
    /// a stack through it merges every member below.
    PullRequest { number: u64 },
}

/// Turn an `octocrab` failure from the route `scope` describes into a
/// [`StackApiError`].
fn stack_api_error(scope: StackScope, error: octocrab::Error) -> StackApiError {
    // `octocrab` keeps the response status and GitHub's message on the boxed
    // `GitHubError` behind this variant. It is the only place they survive:
    // converting to `crate::error::Error` reduces both to a string. Every other
    // variant of `octocrab::Error` — including a response whose error body would
    // not parse, which loses the status — has neither to offer.
    if let octocrab::Error::GitHub { source, .. } = &error {
        // GitHub puts the useful wording either in the top-level message or in
        // the `errors` array under a generic "Validation Failed", so both have
        // to be read. The array's entries are opaque JSON values with no
        // consistent shape — one observed refusal has an *array* in `value` —
        // so they are matched and reported as text.
        let message = joined_error_message(
            &source.message,
            source.errors.as_deref().unwrap_or_default(),
        );

        return classify_stack_api_error(source.status_code.as_u16(), &message, scope);
    }

    StackApiError::Other(error)
}

/// `message` with every entry of `details` appended.
///
/// Generic over `Display` rather than taking `serde_json::Value`, which is what
/// `octocrab` actually hands over: `serde_json` is a dev-dependency only, so it
/// cannot be named here. `Display` is all the matching needs.
fn joined_error_message<D: std::fmt::Display>(message: &str, details: &[D]) -> String {
    let mut joined = message.to_string();

    for detail in details {
        joined.push(' ');
        joined.push_str(&detail.to_string());
    }

    joined
}

/// Classify a stacks endpoint refusal by its status and GitHub's message.
///
/// Classification is total: anything unrecognised becomes
/// [`StackApiError::Rejected`] with the status and message intact, so no
/// information is lost by failing to match.
///
/// This is kept apart from [`stack_api_error`] so that it can be tested:
/// `octocrab`'s `GitHubError` is `#[non_exhaustive]`, so no test outside
/// `octocrab` can build the `octocrab::Error` that `stack_api_error` takes.
fn classify_stack_api_error(status: u16, message: &str, scope: StackScope) -> StackApiError {
    match status {
        404 => match scope {
            StackScope::Repository => StackApiError::NotEnabled,
            StackScope::Stack { stack_number } => StackApiError::StackNotFound { stack_number },
            StackScope::PullRequest { number } => StackApiError::MergeAsyncNotFound { number },
        },
        422 => classify_stack_validation_error(message).unwrap_or(StackApiError::Rejected {
            status,
            message: message.to_string(),
        }),
        _ => StackApiError::Rejected {
            status,
            message: message.to_string(),
        },
    }
}

/// Classify a 422 by GitHub's validation message, or `None` if it says nothing
/// recognisable.
///
/// The messages come from two different layers and are matched on substrings,
/// never in full. All wording below was observed live on 2026-07-31; the older
/// wording that the `gh-stack` reference matches on ("Stack must contain at
/// least two pull requests", "are already stacked") is *not* what GitHub sends
/// today.
fn classify_stack_validation_error(message: &str) -> Option<StackApiError> {
    let message_lowercase = message.to_lowercase();

    // "Pull requests #73, #74 are already part of a stack", and a singular
    // "Pull request #77 is already part of a stack".
    if message_lowercase.contains("already part of a stack") {
        Some(StackApiError::AlreadyStacked {
            pull_requests: pull_request_numbers_in(message),
        })
    // "Pull requests must form a stack, where each PR's base ref is the
    // previous PR's head ref".
    } else if message_lowercase.contains("must form a stack") {
        Some(StackApiError::NotAChain)
    // The minimum is enforced a layer lower, by JSON schema, which words it as
    // "Invalid request.\n\nInvalid property /pull_requests: 2 items required;
    // only 1 was supplied."
    } else if message_lowercase.contains("/pull_requests")
        && message_lowercase.contains("items required")
    {
        Some(StackApiError::TooFewPullRequests)
    } else {
        None
    }
}

/// The pull request numbers written as `#123` in `message`, in the order they
/// appear.
fn pull_request_numbers_in(message: &str) -> Vec<u64> {
    lazy_regex::regex!(r#"#(\d+)"#)
        .captures_iter(message)
        .filter_map(|captures| captures[1].parse().ok())
        .collect()
}

/// The request body every stack-mutating endpoint takes.
#[derive(serde::Serialize)]
struct StackPullRequestNumbers<'a> {
    pull_requests: &'a [u64],
}

/// The first stack in `stacks` that is still open.
///
/// Separate from the call that fetches them so the filter can be tested: the
/// list endpoints return closed stacks alongside open ones, and acting on a
/// closed one is the mistake this exists to prevent.
fn first_open_stack(stacks: Vec<Stack>) -> Option<Stack> {
    stacks.into_iter().find(|stack| stack.open)
}

/// The route to the repository's stacks.
fn stacks_route(config: &crate::config::Config) -> String {
    format!("/repos/{}/{}/stacks", config.owner, config.repo)
}

/// The route to the asynchronous merge of one pull request.
///
/// Not under `/stacks` at all, which is worth knowing before looking for it
/// there: GitHub hangs its stack merge off the pull request that is to be the
/// top of what gets merged.
fn merge_async_route(config: &crate::config::Config, number: u64) -> String {
    format!(
        "/repos/{}/{}/pulls/{number}/merge-async",
        config.owner, config.repo
    )
}

/// The route to one stack, or to `action` on it (`add`, `unstack`).
///
/// The stack is addressed by its number, never by [`Stack::id`].
fn stack_route(config: &crate::config::Config, stack_number: u64, action: Option<&str>) -> String {
    let mut route = format!("{}/{}", stacks_route(config), stack_number);

    if let Some(action) = action {
        route.push('/');
        route.push_str(action);
    }

    route
}

impl GitHub {
    // GitHub's Stacked Pull Requests API. `merge-async`, which merges a whole
    // stack up to a chosen pull request, is [`GitHub::merge_pull_request_async`]
    // below — and under two of the three base strategies it destroys the pull
    // requests above the one it merges, which is why `land` asks whether the
    // branches survive a rebase before it calls it.
    //
    // After the merge GitHub repoints the next survivor at the stack's base and
    // force-pushes that survivor's head branch, rebasing it onto the new base.
    // Under `spr.baseStrategy = synthetic` or `linear` a pull request branch
    // jj-spr pushes is a *merge* commit (`pr_head_parents` in `commands::diff`),
    // and a linear rebase replays only the non-merge commits of the range — all
    // of which are on the master branch by then, after the squash. So the branch
    // collapses onto its base, GitHub sees a pull request with no changes, and
    // closes it, review and all. Established end to end against a live
    // repository on 2026-07-31, while this `land` was being written: merging the
    // middle of a stack of three closed the top one and reset its head branch to
    // the tip of the master branch. It happens with `merge_action` `default` and
    // `direct_merge` alike, and whether or not the merged pull request's head
    // branch is deleted afterwards. Ordinary single-parent branches survive the
    // same treatment, which is why GitHub's own client does not hit this, and
    // why an earlier probe of the API — which built its own plain branches — did
    // not find it: the hazard is specific to those two strategies' branch shape.
    //
    // `spr.baseStrategy = linear-rebase` exists to build the branches that do
    // survive it, so under that strategy the merge is safe — from GitHub's own
    // interface, and from `jj spr land --stack`, which is what
    // [`GitHub::merge_pull_request_async`] is for. Without it, `land` takes the
    // stack apart and merges the pull requests one at a time, which lands the
    // same changes as their own commits under their own titles and needs no
    // strategy to be true.
    //
    // The endpoint is a `PUT`. A `POST` to the same path gets an ordinary
    // route-not-found 404 that is easy to misread as this repository not having
    // stacks enabled.

    /// Every stack in the repository.
    ///
    /// This includes stacks that are no longer open, which GitHub keeps
    /// indefinitely once they hold a merged pull request. Filter on
    /// [`Stack::open`] before acting on any of them. GitHub's own client says
    /// this comes back ordered by stack number descending; no observation
    /// confirms it, since the probe never had more than one stack to order, so
    /// nothing should depend on the order.
    pub async fn get_stacks(&self) -> StackResult<Vec<Stack>> {
        self.get_stacks_matching(&[]).await
    }

    /// Every stack GitHub associates with pull request `number`.
    ///
    /// A *merged* pull request keeps showing up here, in the closed stack it was
    /// merged in, because GitHub keeps that stack forever. One whose stack was
    /// dissolved instead answers with nothing: the record is gone. So a result
    /// here does not mean there is a stack to work with, and anything that means
    /// to *act* on one wants [`GitHub::get_open_stack_for_pull_request`] —
    /// adopting the first entry of this list would latch onto a dead stack.
    pub async fn get_stacks_for_pull_request(&self, number: u64) -> StackResult<Vec<Stack>> {
        // `GET /repos/{owner}/{repo}/stacks?pull_request=N`.
        self.get_stacks_matching(&[("pull_request", number.to_string())])
            .await
    }

    /// The open stack that pull request `number` belongs to, or `None` when it
    /// is not in one.
    ///
    /// A pull request is in at most one *open* stack, so the single answer is
    /// the whole answer rather than a choice among several. GitHub enforces
    /// this rather than merely happening to satisfy it: asking it to put a pull
    /// request that is already in an open stack into a second one is refused
    /// with `422 Pull request #N is already part of a stack`.
    ///
    /// The list this narrows can still hold more than one entry, which is why
    /// [`first_open_stack`] filters rather than taking whatever came first: a
    /// stack that ever held a merged pull request is never deleted, and it goes
    /// on listing its members, so a pull request accumulates closed stacks for
    /// as long as the repository lives.
    pub async fn get_open_stack_for_pull_request(&self, number: u64) -> StackResult<Option<Stack>> {
        Ok(first_open_stack(
            self.get_stacks_for_pull_request(number).await?,
        ))
    }

    /// The stacks list endpoint, narrowed by `filter`.
    ///
    /// Paginated for the filtered form as much as the unfiltered one — it is one
    /// endpoint, and the cost of following a `Link` header that is never there
    /// is nothing.
    async fn get_stacks_matching(&self, filter: &[(&str, String)]) -> StackResult<Vec<Stack>> {
        let octocrab = octocrab::instance();
        let scope = StackScope::Repository;

        // GitHub's default page size is 30 and it never forgets a stack that
        // holds a merged pull request, so a repository that has used stacks for
        // a while would otherwise be walked 30 at a time on every call.
        let mut query = filter.to_vec();
        query.push(("per_page", "100".to_string()));

        let first_page = octocrab
            .get::<octocrab::Page<Stack>, _, _>(stacks_route(&self.config), Some(&query))
            .await
            .on_stack_scope(scope)?;

        octocrab.all_pages(first_page).await.on_stack_scope(scope)
    }

    /// The stack numbered `stack_number`.
    pub async fn get_stack(&self, stack_number: u64) -> StackResult<Stack> {
        let scope = StackScope::Stack { stack_number };

        octocrab::instance()
            .get::<Stack, _, _>(stack_route(&self.config, stack_number, None), None::<&()>)
            .await
            .on_stack_scope(scope)
    }

    /// Make a stack out of `pull_requests`, ordered bottom to top.
    ///
    /// GitHub requires at least two pull requests, and requires that each one's
    /// base ref is the previous one's head ref — see [`StackApiError`] for what
    /// it says when they do not.
    pub async fn create_stack(&self, pull_requests: &[u64]) -> StackResult<Stack> {
        self.post_pull_requests(
            StackScope::Repository,
            stacks_route(&self.config),
            pull_requests,
        )
        .await
    }

    /// Append `pull_requests` to the top of stack `stack_number`, ordered
    /// bottom to top.
    ///
    /// This takes only the pull requests that are not in the stack yet, and
    /// only ever appends: the API cannot insert into the middle of a stack,
    /// reorder one, or remove a single pull request. Restructuring means
    /// [`GitHub::unstack`] followed by [`GitHub::create_stack`].
    ///
    /// Closed and merged members below do not get in the way — a stack goes on
    /// accepting pull requests after part of it has landed.
    pub async fn add_to_stack(
        &self,
        stack_number: u64,
        pull_requests: &[u64],
    ) -> StackResult<Stack> {
        self.post_pull_requests(
            StackScope::Stack { stack_number },
            stack_route(&self.config, stack_number, Some("add")),
            pull_requests,
        )
        .await
    }

    async fn post_pull_requests(
        &self,
        scope: StackScope,
        route_path: String,
        pull_requests: &[u64],
    ) -> StackResult<Stack> {
        octocrab::instance()
            .post::<StackPullRequestNumbers, Stack>(
                route_path,
                Some(&StackPullRequestNumbers { pull_requests }),
            )
            .await
            .on_stack_scope(scope)
    }

    /// Take every pull request out of stack `stack_number`, releasing the lock
    /// the stack holds on their base refs.
    ///
    /// This is the only way to remove anything from a stack: there is no
    /// per-pull-request removal, and passing a body naming some of them is
    /// ignored rather than honoured. See [`UnstackOutcome`] for the two ways it
    /// can land. It leaves every base ref exactly as it found it, so a caller
    /// that wants the pull requests retargeted has to do that itself afterwards.
    pub async fn unstack(&self, stack_number: u64) -> StackResult<UnstackOutcome> {
        let octocrab = octocrab::instance();
        let scope = StackScope::Stack { stack_number };

        // Not `Octocrab::post`, which always deserializes a response body:
        // GitHub answers 204 with no body when the stack is gone.
        let response = octocrab
            ._post(
                stack_route(&self.config, stack_number, Some("unstack")),
                None::<&()>,
            )
            .await
            .on_stack_scope(scope)?;
        let response = octocrab::map_github_error(response)
            .await
            .on_stack_scope(scope)?;

        if response.status().as_u16() == 204 {
            return Ok(UnstackOutcome::Dissolved);
        }

        let stack = <Stack as octocrab::FromResponse>::from_response(response)
            .await
            .on_stack_scope(scope)?;

        Ok(UnstackOutcome::Retained(stack))
    }

    /// Ask GitHub to squash-merge pull request `number` and, where a stack holds
    /// it, every member below it — the merge its own interface offers on a stack.
    ///
    /// Asynchronous, hence the name: GitHub takes the request and answers before
    /// anything is merged, so a caller that needs the result has to watch the
    /// pull requests. Observed to take a few seconds for a stack of three.
    ///
    /// What it does, established against a live repository on 2026-08-07 rather
    /// than read out of documentation, which describes none of it:
    ///
    /// - **It merges downwards, one commit per pull request.** Merging the middle
    ///   of a stack of three merged the bottom and the middle, bottom first, as
    ///   one squash commit each, and left the top open. There is no way to ask it
    ///   for fewer: how far *up* to go is the only choice it offers.
    /// - **The pull requests above are retargeted and rebased.** The survivor
    ///   came out based on the master branch with its head branch force-pushed
    ///   onto the new tip, showing only its own change and mergeable. That is the
    ///   work `land` otherwise leaves to the next `jj spr diff` — and the reason
    ///   this endpoint is only usable under `spr.baseStrategy = linear-rebase`:
    ///   the account of what the rebase does to a branch built out of merge
    ///   commits is at the top of this `impl`.
    /// - **The stack survives.** It stays open, still holding every member, the
    ///   merged ones marked merged. Nothing has to be dissolved to merge this
    ///   way, and nothing has to be registered again afterwards.
    /// - **The merged branches stay.** Deleting them is the caller's to do.
    ///
    /// Squash and only squash, which is not a parameter for the reason set out
    /// where `land` merges a pull request on its own: one Jujutsu change lands as
    /// one commit. Unlike that merge, the commit message cannot be chosen — one
    /// request merges several pull requests, so GitHub words each of them from
    /// the repository's own squash settings.
    ///
    /// `expected_head_sha` is not sent, and would buy nothing if it were: the
    /// endpoint accepts the field, echoes the pull request's *real* head back in
    /// it, and merges regardless. A run of the probe passed forty zeroes and the
    /// merge went ahead. So this is not a leased merge, unlike the one `land`
    /// makes itself, which pins the head it merges.
    ///
    /// A 409 is not a failure. It says a merge request for this pull request is
    /// already in flight, which is the merge the caller is asking for; the status
    /// alone says so, so nothing here reads GitHub's wording for it.
    pub async fn merge_pull_request_async(&self, number: u64) -> StackResult<AsyncMerge> {
        let octocrab = octocrab::instance();
        let scope = StackScope::PullRequest { number };

        // A `PUT`. A `POST` to the same path gets an ordinary route-not-found
        // 404, which is easy to misread as this repository not having stacks.
        let response = octocrab
            ._put(
                merge_async_route(&self.config, number),
                Some(&AsyncMergeRequest {
                    merge_method: "squash",
                }),
            )
            .await
            .on_stack_scope(scope)?;

        let response = match octocrab::map_github_error(response)
            .await
            .on_stack_scope(scope)
        {
            Ok(response) => response,
            Err(StackApiError::Rejected { status: 409, .. }) => {
                return Ok(AsyncMerge::AlreadyEnqueued);
            }
            Err(error) => return Err(error),
        };

        let answer = <AsyncMergeResponse as octocrab::FromResponse>::from_response(response)
            .await
            .on_stack_scope(scope)?;

        Ok(async_merge(&answer.status, answer.details.sha))
    }
}

/// Read a merge request's answer, given GitHub's `status` for it.
///
/// Only `merged` is an end state, so anything else is taken to mean the merge is
/// still coming — including a status this build does not know, which is the
/// reading that costs a caller some waiting rather than a merge it thinks
/// happened and did not.
///
/// Apart so that it can be tested: the statuses are GitHub's, and this is the
/// one place they are interpreted.
fn async_merge(status: &str, sha: Option<String>) -> AsyncMerge {
    match status {
        "merged" => AsyncMerge::Merged { sha },
        _ => AsyncMerge::Enqueued,
    }
}

/// Tests for the Stacked Pull Requests client.
///
/// Every message here that purports to be GitHub's own wording, and every JSON
/// fixture, was captured off the live API on 2026-07-31 rather than transcribed
/// from the docs — so these are the assertions that pin the client to what
/// GitHub actually sends. The unrecognised-refusal tests use deliberately
/// synthetic text, which is the point of them. Wording that only the `gh-stack`
/// reference implementation uses is absent on purpose: GitHub does not send it.
#[cfg(test)]
mod stack_tests {
    use super::*;

    const REPOSITORY: StackScope = StackScope::Repository;
    const STACK: StackScope = StackScope::Stack { stack_number: 76 };

    fn config() -> crate::config::Config {
        crate::config::Config::new(
            "acme".into(),
            "widgets".into(),
            "origin".into(),
            "main".into(),
            "spr/".into(),
            false,
        )
    }

    /// The asynchronous merge lives on the pull request, so a 404 there says
    /// nothing about the repository having stacks — unlike the same status on the
    /// repository's own stacks route, which is what callers degrade on.
    #[test]
    fn a_missing_async_merge_route_is_not_a_repository_without_stacks() {
        let error =
            classify_stack_api_error(404, "Not Found", StackScope::PullRequest { number: 42 });

        assert!(
            matches!(error, StackApiError::MergeAsyncNotFound { number: 42 }),
            "{error:?}"
        );
    }

    /// A merge already in flight comes back as 409, and the status alone says so.
    /// Kept as a `Rejected` by the classifier — it is
    /// [`GitHub::merge_pull_request_async`] that reads it as the merge it asked
    /// for — so what this pins is that the status survives to be read.
    #[test]
    fn a_merge_already_in_flight_keeps_its_status() {
        let error = classify_stack_api_error(
            409,
            "A merge request already exists for this pull request.",
            StackScope::PullRequest { number: 42 },
        );

        assert!(
            matches!(error, StackApiError::Rejected { status: 409, .. }),
            "{error:?}"
        );
    }

    /// Only `merged` means the merge has happened. Anything else, including a
    /// status this build has never seen, leaves the caller waiting — which costs
    /// time, where the other reading would cost a merge that never happened being
    /// treated as done.
    #[test]
    fn only_a_merged_status_ends_an_async_merge() {
        assert_eq!(
            async_merge("merged", Some("abc123".to_string())),
            AsyncMerge::Merged {
                sha: Some("abc123".to_string())
            }
        );
        assert_eq!(async_merge("pending", None), AsyncMerge::Enqueued);
        assert_eq!(
            async_merge("something-new", None),
            AsyncMerge::Enqueued,
            "an unknown status must not read as merged"
        );
    }

    /// The body the merge request sends. `merge_method` is the property GitHub
    /// keys on, and it ignores properties it does not know — so a wrong name
    /// would silently merge by the repository's default method instead of
    /// squashing.
    #[test]
    fn test_async_merge_request_body_shape() {
        assert_eq!(
            serde_json::to_string(&AsyncMergeRequest {
                merge_method: "squash"
            })
            .unwrap(),
            r#"{"merge_method":"squash"}"#
        );
    }

    #[test]
    fn test_request_body_shape() {
        // The one thing this client *sends*. `pull_requests` is the property
        // name the schema layer keys on, and GitHub ignores properties it does
        // not know, so getting it wrong would not reliably fail loudly.
        assert_eq!(
            serde_json::to_string(&StackPullRequestNumbers {
                pull_requests: &[73, 74]
            })
            .unwrap(),
            r#"{"pull_requests":[73,74]}"#
        );
    }

    #[test]
    fn test_routes() {
        // Nothing else pins these: the routes are only exercised by a real call.
        assert_eq!(stacks_route(&config()), "/repos/acme/widgets/stacks");
        assert_eq!(
            stack_route(&config(), 76, None),
            "/repos/acme/widgets/stacks/76"
        );
        assert_eq!(
            stack_route(&config(), 76, Some("add")),
            "/repos/acme/widgets/stacks/76/add"
        );
        assert_eq!(
            stack_route(&config(), 76, Some("unstack")),
            "/repos/acme/widgets/stacks/76/unstack"
        );
    }

    #[test]
    fn test_classify_not_found_by_route() {
        // The same status means different things on the two kinds of route, and
        // only one of them is a reason to stop using stacks for the repository.
        assert!(matches!(
            classify_stack_api_error(404, "Not Found", REPOSITORY),
            StackApiError::NotEnabled
        ));
        assert!(matches!(
            classify_stack_api_error(404, "Not Found", STACK),
            StackApiError::StackNotFound { stack_number: 76 }
        ));
    }

    #[test]
    fn test_classify_too_few_pull_requests() {
        // Enforced by JSON schema, not by the stacks logic, hence the wording.
        assert!(matches!(
            classify_stack_api_error(
                422,
                "Invalid request.\n\nInvalid property /pull_requests: 2 items required; only 1 \
                 was supplied.",
                REPOSITORY,
            ),
            StackApiError::TooFewPullRequests
        ));
    }

    #[test]
    fn test_classify_pull_requests_not_a_chain() {
        assert!(matches!(
            classify_stack_api_error(
                422,
                "Pull requests must form a stack, where each PR's base ref is the previous PR's \
                 head ref",
                REPOSITORY,
            ),
            StackApiError::NotAChain
        ));
    }

    #[test]
    fn test_classify_already_stacked_keeps_the_pull_request_numbers() {
        let error = classify_stack_api_error(
            422,
            "Pull requests #73, #74, #75 are already part of a stack",
            REPOSITORY,
        );

        match error {
            StackApiError::AlreadyStacked { pull_requests } => {
                assert_eq!(pull_requests, vec![73, 74, 75]);
            }
            other => panic!("expected AlreadyStacked, got {other:?}"),
        }
    }

    #[test]
    fn test_classify_already_stacked_in_the_singular() {
        // GitHub pluralizes the sentence, so the match cannot include the verb.
        match classify_stack_api_error(422, "Pull request #77 is already part of a stack", STACK) {
            StackApiError::AlreadyStacked { pull_requests } => {
                assert_eq!(pull_requests, vec![77]);
            }
            other => panic!("expected AlreadyStacked, got {other:?}"),
        }
    }

    #[test]
    fn test_classify_reads_the_nested_errors_array() {
        // "these pull requests do not exist" arrives as a generic top-level
        // message with the detail in `errors`, so classification has to see the
        // joined text to report anything useful.
        // Built as a `serde_json::Value` because that is what `octocrab` hands
        // over, and its `Display` is the whole reason the array can be read at
        // all without taking a runtime dependency on `serde_json`.
        let detail = serde_json::json!({
            "value": [99998, 99999],
            "resource": "Stack",
            "field": "pull_requests",
            "code": "missing"
        });
        let message = joined_error_message("Validation Failed", &[detail]);

        match classify_stack_api_error(422, &message, REPOSITORY) {
            StackApiError::Rejected { status, message } => {
                assert_eq!(status, 422);
                assert!(message.contains("Validation Failed"));
                assert!(message.contains(r#""code":"missing""#));
                assert!(message.contains("99998"));
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn test_classify_preserves_anything_it_does_not_recognise() {
        // The messages are prose and have been reworded before, so an unmatched
        // refusal has to keep enough to report rather than collapse to "failed".
        match classify_stack_api_error(422, "Some brand new refusal", REPOSITORY) {
            StackApiError::Rejected { status, message } => {
                assert_eq!(status, 422);
                assert_eq!(message, "Some brand new refusal");
            }
            other => panic!("expected Rejected, got {other:?}"),
        }

        // Only 404 says anything about whether stacks are available; 403 is an
        // ordinary refusal and must not switch stacking off.
        match classify_stack_api_error(403, "Resource not accessible", REPOSITORY) {
            StackApiError::Rejected { status, .. } => assert_eq!(status, 403),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn test_pull_request_numbers_in_message() {
        // Order follows the message, which echoes the request, not the stack.
        assert_eq!(pull_request_numbers_in("#75, #74, #73"), vec![75, 74, 73]);
        assert!(pull_request_numbers_in("no numbers here").is_empty());
    }

    /// A composite of the captured `probe-evidence/03-detail.json`: real key
    /// shapes throughout, with the member states edited so that one member is
    /// merged and the other an open draft. The first member deliberately keeps
    /// the fields this client does not model, to prove they are ignored; the
    /// second keeps only what is modelled.
    fn stack_json() -> &'static str {
        r#"{
            "id": 86543,
            "number": 76,
            "node_id": "PRS_kwDOM1EEbM4AAVIP",
            "url": "https://api.github.com/repos/acme/widgets/stacks/76",
            "base": { "ref": "main" },
            "open": true,
            "created_at": "2026-07-31T16:41:19Z",
            "pull_requests": [
                {
                    "url": "https://api.github.com/repos/acme/widgets/pulls/73",
                    "id": 4179389944,
                    "number": 73,
                    "head": {
                        "ref": "probe/a",
                        "sha": "ee6dc38b1ef12869c441133226c35e146afd2666",
                        "repo": { "id": 860947564, "url": "https://api.github.com/repos/acme/widgets", "name": "widgets" }
                    },
                    "base": {
                        "ref": "main",
                        "sha": "47d8b8320224350b17f49a2a54fd3b5e27407dd1",
                        "repo": { "id": 860947564, "url": "https://api.github.com/repos/acme/widgets", "name": "widgets" }
                    },
                    "node_id": "PR_kwDOM1EEbM75HG34",
                    "title": "probe A",
                    "state": "closed",
                    "merged_at": "2026-07-31T16:44:19Z",
                    "draft": false,
                    "html_url": "https://github.com/acme/widgets/pull/73",
                    "user": { "login": "quodlibetor", "id": 277161, "site_admin": false }
                },
                {
                    "number": 74,
                    "head": { "ref": "probe/b", "sha": "1254342174442b83dafa0377c4afb2af125f884e" },
                    "base": { "ref": "probe/a", "sha": "ee6dc38b1ef12869c441133226c35e146afd2666" },
                    "title": "probe B",
                    "state": "open",
                    "merged_at": null,
                    "draft": true
                }
            ]
        }"#
    }

    #[test]
    fn test_deserialize_stack() {
        let stack: Stack = serde_json::from_str(stack_json()).unwrap();

        // `id` and `number` are different fields; the paths take `number`.
        assert_eq!(stack.id, 86543);
        assert_eq!(stack.number, 76);
        // The stack's own base has a ref and no SHA, unlike a member's.
        assert_eq!(stack.base.ref_name, "main");
        assert!(stack.open);
        assert_eq!(stack.pull_request_numbers(), vec![73, 74]);

        let bottom = &stack.pull_requests[0];
        assert_eq!(bottom.state, StackPullRequestState::Closed);
        // There is no "merged" state — a merged member is closed with a date.
        assert!(bottom.is_merged());
        assert_eq!(bottom.head.ref_name, "probe/a");
        assert_eq!(
            bottom.head.sha.as_deref(),
            Some("ee6dc38b1ef12869c441133226c35e146afd2666")
        );
        assert_eq!(bottom.title.as_deref(), Some("probe A"));

        let top = &stack.pull_requests[1];
        assert_eq!(top.state, StackPullRequestState::Open);
        assert!(top.draft);
        assert!(!top.is_merged());
        // Ordered bottom to top: the second member is based on the first's head.
        assert_eq!(
            top.base.as_ref().map(|base| base.ref_name.as_str()),
            Some("probe/a")
        );
    }

    #[test]
    fn test_deserialize_stack_list() {
        // The captured `GET /repos/{owner}/{repo}/stacks` body *as it was
        // saved* — it was trimmed before saving, so this says nothing about what
        // the endpoint omits. It is still the reason the member fields below
        // `head` are optional: nothing proves the list form sends them, so
        // requiring one would be a bet rather than an observation.
        let stacks: Vec<Stack> = serde_json::from_str(
            r#"[{"id":86543,"number":76,"node_id":"PRS_kwDOM1EEbM4AAVIP",
                 "url":"https://api.github.com/repos/acme/widgets/stacks/76",
                 "base":{"ref":"main"},"open":true,
                 "created_at":"2026-07-31T16:41:19Z",
                 "pull_requests":[
                   {"number":73,"state":"open","draft":false,"merged_at":null,
                    "head":{"ref":"probe/a","sha":"ee6dc38b1ef12869c441133226c35e146afd2666"}},
                   {"number":74,"state":"open","draft":false,"merged_at":null,
                    "head":{"ref":"probe/b","sha":"1254342174442b83dafa0377c4afb2af125f884e"}},
                   {"number":75,"state":"open","draft":false,"merged_at":null,
                    "head":{"ref":"probe/c","sha":"9f8ef69ced3669d98b8568a404afc4f06706abff"}}]}]"#,
        )
        .unwrap();

        assert_eq!(stacks.len(), 1);
        assert_eq!(stacks[0].number, 76);
        assert_eq!(stacks[0].pull_request_numbers(), vec![73, 74, 75]);

        // Neither of these survives in the saved capture, so neither may be
        // required.
        let bottom = &stacks[0].pull_requests[0];
        assert!(bottom.base.is_none());
        assert!(bottom.title.is_none());
        assert_eq!(bottom.head.ref_name, "probe/a");
    }

    #[test]
    fn test_first_open_stack_skips_closed_ones() {
        // The list endpoints hand back closed stacks alongside open ones, and a
        // pull request that has been landed and restacked collects them, so the
        // closed one can perfectly well come first.
        let closed: Stack = serde_json::from_str(
            r#"{ "id": 1, "number": 70, "base": { "ref": "main" }, "open": false,
                 "pull_requests": [] }"#,
        )
        .unwrap();
        let open: Stack = serde_json::from_str(
            r#"{ "id": 2, "number": 76, "base": { "ref": "main" }, "open": true,
                 "pull_requests": [] }"#,
        )
        .unwrap();

        assert_eq!(
            first_open_stack(vec![closed.clone(), open]).map(|stack| stack.number),
            Some(76)
        );
        assert!(first_open_stack(vec![closed]).is_none());
        assert!(first_open_stack(Vec::new()).is_none());
    }

    #[test]
    fn test_deserialize_stack_that_has_been_unstacked() {
        // What unstacking a stack with merged members answers with: the record
        // survives, closed, holding only the pull requests it could not release.
        let stack: Stack = serde_json::from_str(
            r#"{
                "id": 86543,
                "number": 76,
                "base": { "ref": "main" },
                "open": false,
                "pull_requests": [
                    {
                        "number": 73,
                        "state": "closed",
                        "merged_at": "2026-07-31T16:44:19Z",
                        "head": { "ref": "probe/a", "sha": "ee6dc38b" }
                    }
                ]
            }"#,
        )
        .unwrap();

        assert!(!stack.open);
        assert!(stack.pull_requests[0].is_merged());
    }

    #[test]
    fn test_deserialize_stack_tolerates_a_state_it_does_not_know() {
        // One unfamiliar string must cost this member's state and nothing else:
        // the alternative is losing every stack in the repository to it.
        let stack: Stack = serde_json::from_str(
            r#"{
                "id": 1,
                "number": 1,
                "base": { "ref": "main" },
                "open": true,
                "pull_requests": [
                    { "number": 5, "state": "hypothetical", "head": { "ref": "spr/x" } }
                ]
            }"#,
        )
        .unwrap();

        let pull_request = &stack.pull_requests[0];
        assert_eq!(pull_request.state, StackPullRequestState::Unknown);
        assert_eq!(pull_request.number, 5);
        assert!(pull_request.head.sha.is_none());
        assert!(pull_request.base.is_none());
        assert!(!pull_request.draft);
        assert!(!pull_request.is_merged());
    }
}
