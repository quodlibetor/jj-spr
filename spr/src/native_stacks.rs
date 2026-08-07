/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Registering the pull requests a run pushed as a stack GitHub itself draws,
//! and taking one back out of a stack when something has to be done that a
//! stack does not allow.
//!
//! This is a stage a run passes through, not a way of doing what `diff` does.
//! Nothing here changes what is pushed or what a pull request is based on —
//! a linear `spr.baseStrategy` already builds the chain of base refs that
//! GitHub's Stacked Pull Requests API requires, and this only tells GitHub that
//! the chain is one. So it is plain functions over a session value that each
//! command holds as an [`Option`]: `None` is the whole of "the feature is off",
//! and no call site asks what mode it is in.
//!
//! Two facts about the API shape everything here, both established by probing
//! the live API on 2026-07-31 rather than read out of GitHub's documentation,
//! which states neither:
//!
//! - **A stack owns its members' base refs.** Any `PATCH` to a member carrying
//!   a `base` field is refused, whether or not the value differs. So a run that
//!   moves a base has to take the pull request out of its stack first — see
//!   [`StackSession::unlock_base`].
//! - **A stack is append-only and outlives its members.** There is no way to
//!   remove one pull request, reorder, or insert. A closed member stays in
//!   place, and a merged one stays even through an unstack — a stack that has
//!   held a merged pull request can never be deleted, only closed. Restructuring
//!   means dissolving the stack and making a new one, which mints a new stack
//!   number, so `plan` works hard to avoid it.
//!
//! A third fact is a consequence of the second and is worth stating on its own,
//! because it is what makes this more than a one-shot registration: because
//! `/add` only ever appends, a stack cannot grow *downwards*. A run that pushes
//! a chain reaching below what a stack already holds cannot be added to it, and
//! has to dissolve it.

use std::collections::HashSet;

use crate::{
    error::{Error, Result},
    github::{GitHubApi, Stack, StackApiError, StackResult, StackedPullRequest, UnstackOutcome},
    output::output,
};

/// One change a run dealt with, as the stack registration sees it.
///
/// The run's changes are walked bottom-up, so a list of these is a description
/// of the whole run in the order GitHub wants a stack in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChainLink {
    /// The pull request the change has, where it has one.
    ///
    /// A change whose pull request the run is about to open has none until it
    /// is opened, and a dry run never opens one. Such a change breaks the
    /// chain: nothing above it can name what it is based on.
    pub pull_request: Option<u64>,

    /// The pull request this change's own is based on, where the link GitHub
    /// requires actually holds — that is, where this pull request's base ref is
    /// that pull request's head ref.
    ///
    /// `None` is a break in the chain rather than a fact about the change: the
    /// bottom of a run always has it, and so does any change whose pull request
    /// is against the master branch, a synthetic base branch, or a branch that
    /// this run did not push.
    pub based_on: Option<u64>,

    /// Whether the run moves this pull request's base ref, which no stack
    /// holding it can survive — see [`StackSession::unlock_base`].
    ///
    /// A real run has already taken that stack apart by the time it registers,
    /// so it is simply not there to be found. A dry run has moved nothing, so
    /// it is, and it has to be set aside deliberately or the run would be
    /// reported as leaving a stack alone that it is about to destroy.
    pub retargeted: bool,
}

/// A run of pull requests that GitHub could hold as one stack.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Chain {
    /// The pull requests, bottom to top.
    pub pull_requests: Vec<u64>,

    /// Those of them whose base ref the run moves.
    ///
    /// Deliberately the pull requests rather than a flag for the chain: only a
    /// stack holding one of *these* has to come apart. A chain can perfectly
    /// well have its top retargeted onto a stack that is otherwise untouched
    /// and can simply be appended to, and a flag covering the whole chain would
    /// destroy that stack for nothing.
    pub retargeted: Vec<u64>,
}

/// Split a run into the chains of pull requests GitHub could hold as stacks.
///
/// `links` is the run bottom-up. A chain runs on for as long as each pull
/// request is based on the one before it, and starts afresh wherever that stops
/// being true — which is exactly where a `POST /stacks` spanning the break
/// would be refused for not forming a stack.
///
/// Chains of one are returned like any other: whether one pull request is worth
/// reporting on is the caller's question, not this one's.
pub fn chains(links: &[ChainLink]) -> Vec<Chain> {
    let mut chains: Vec<Chain> = Vec::new();
    let mut current = Chain::default();

    // Start a fresh chain, keeping the finished one if it holds anything.
    fn cut(chains: &mut Vec<Chain>, current: &mut Chain) {
        if !current.pull_requests.is_empty() {
            chains.push(std::mem::take(current));
        }
    }

    for link in links {
        let Some(number) = link.pull_request else {
            // Nothing above a change without a pull request can be based on
            // it, so the chain cannot continue through one.
            cut(&mut chains, &mut current);
            continue;
        };

        // `based_on` naming some *other* pull request is a break too, not just
        // a `None`: the run was given revisions that do not form one chain.
        let continues =
            link.based_on.is_some() && link.based_on == current.pull_requests.last().copied();

        if !continues {
            cut(&mut chains, &mut current);
        }

        current.pull_requests.push(number);
        // After the cut, so that a retargeted link that starts a chain marks
        // the chain it starts rather than the one it ended.
        if link.retargeted {
            current.retargeted.push(number);
        }
    }

    cut(&mut chains, &mut current);

    chains
}

/// What is to become of one chain of pull requests, or what became of it.
///
/// The variants that make a stack carry the new stack's number once there is
/// one, so the same value describes a plan a dry run is reporting and the work
/// a real run has done.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reconciliation {
    /// Fewer than two pull requests were chained, and GitHub has no such thing
    /// as a stack of one.
    NotAStack,

    /// The repository has not opted in to stacked pull requests, so there is
    /// nothing to register with.
    Unsupported,

    /// A stack already holds this chain, in this order.
    UpToDate { stack_number: u64 },

    /// A stack already holds the bottom of this chain, and the rest goes on top
    /// of it.
    Append {
        stack_number: u64,
        pull_requests: Vec<u64>,
    },

    /// No stack holds these pull requests, so a new one does.
    Create {
        /// The stack that was made, once it has been.
        stack_number: Option<u64>,
    },

    /// Stacks hold these pull requests in some other shape, and the API can
    /// neither reorder a stack nor take anything out of one, so they are
    /// dissolved and a new stack takes the chain.
    ///
    /// More than one stack can be in the way at once: the chain the run pushed
    /// may span pull requests that ended up in different stacks, and every one
    /// of those has to go before a stack can hold the chain.
    Recreate {
        /// The stacks that were dissolved, ascending.
        dissolved: Vec<u64>,
        /// The stack that was made, once it has been.
        stack_number: Option<u64>,
    },

    /// Stacks the run will take apart before it can retarget anything, on a dry
    /// run.
    ///
    /// A real run says this as it happens — `diff` prints it as it unstacks —
    /// and has no use for it here. A dry run moves nothing, so without this the
    /// one irreversible thing the feature does would go unmentioned whenever
    /// every member of the stack ends up in a new one and nothing is reported
    /// as lost.
    Dissolving { stack_numbers: Vec<u64> },

    /// Pull requests this run took out of a stack and did not put back into
    /// one.
    ///
    /// Taking one pull request out of a stack takes the whole stack apart, and
    /// only the pull requests the run pushed can be registered again — so a run
    /// that moves a base leaves behind every member of that stack it did not
    /// itself push, and a run that fails before registering leaves behind all
    /// of them. That is not recoverable and not what the user asked for, so it
    /// is said out loud rather than left to be noticed on GitHub.
    ///
    /// Unlike the others this is not one chain's answer: it is the whole run's,
    /// from [`StackSession::orphaned_pull_requests`], because a pull request is
    /// only really lost once every chain has had its chance to register it.
    Orphaned { pull_requests: Vec<u64> },
}

impl Reconciliation {
    /// A phrase naming the work, which reads the same whether it has been done
    /// or is only being reported by `--dry-run`.
    pub fn describe(&self) -> String {
        match self {
            Self::NotAStack => {
                "no stack: GitHub's stacks take at least two chained pull requests".to_string()
            }
            Self::Unsupported => {
                "no stack: this repository does not have stacked pull requests enabled".to_string()
            }
            Self::UpToDate { stack_number } => {
                format!("stack #{stack_number}, unchanged")
            }
            Self::Append {
                stack_number,
                pull_requests,
            } => format!("{} on top of stack #{stack_number}", numbers(pull_requests)),
            Self::Create { stack_number } => match stack_number {
                Some(number) => format!("a new stack, #{number}"),
                None => "a new stack".to_string(),
            },
            Self::Recreate {
                dissolved,
                stack_number,
            } => match stack_number {
                Some(number) => format!(
                    "{} dissolved, and stack #{number} in its place",
                    stacks(dissolved)
                ),
                None => format!(
                    "{} dissolved, and a new stack in its place",
                    stacks(dissolved)
                ),
            },
            Self::Dissolving { stack_numbers } => format!(
                "{} taken apart first, because the run moves the base of a pull request in it \
                 and GitHub does not allow that while a stack holds it. What this run pushes is \
                 registered again as a new stack, with a new number",
                stacks(stack_numbers),
            ),
            Self::Orphaned { pull_requests } => format!(
                "{} left unstacked. A stack cannot be reshaped in place, so reshaping one \
                 means taking it apart and registering what this run pushed as a new one — \
                 and these were in it but are not in that. Pushing the whole stack in one run \
                 puts back any of them that still have a change in it",
                numbers(pull_requests),
            ),
        }
    }

    /// Whether this is worth saying out loud on a run that is not a dry run.
    ///
    /// A run that pushed a single change is not a stack, and a stack that is
    /// already right is not news; both are reported when a dry run is asked
    /// what would happen and not otherwise. A repository that turns out not to
    /// have stacked pull requests is a setting that is not doing what it says,
    /// and that is worth a word every time.
    pub fn is_notable(&self) -> bool {
        !matches!(self, Self::NotAStack | Self::UpToDate { .. })
    }
}

/// `#12, #13`.
fn numbers(pull_requests: &[u64]) -> String {
    pull_requests
        .iter()
        .map(|number| format!("#{number}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// What `land` says about the pull requests it took out of a stack and left out
/// of one, or `None` where it left none.
///
/// `land`'s and no one else's: the sentence names the land, and tells the reader
/// to rebase because a land has just moved the ground under everything above it.
/// [`left_unstacked_by_a_close`] is the sibling this shape asks for rather than
/// a general one taking the command word as a parameter — the whole point of
/// keeping these sentences in this module is that they read as one voice about
/// one subject, and a word threaded in from the call site would be prose
/// assembled there again.
///
/// Kept here rather than at the call site for the same reason
/// [`DissolveReason`] is: every sentence jj-spr says about a stack is written in
/// this module, in one voice. [`Reconciliation::Orphaned`] says the same thing
/// in `diff`'s words, which are about what a run pushed and registered — `land`
/// pushes and registers nothing, so it needs its own.
///
/// Plural throughout: a land dissolves the stack holding the pull request it is
/// landing *and* any stack holding one of the pull requests above it, so what
/// comes loose can span several stacks and can never be put back as one.
pub fn left_unstacked_by_a_land(pull_requests: &[u64]) -> Option<String> {
    (!pull_requests.is_empty()).then(|| {
        format!(
            "GitHub stack: {} left unstacked by the stacks this land took apart. Nothing puts a \
             stack back but `jj spr diff`, so rebase and push the ones you still want stacked — \
             `jj spr diff --all -r 'trunk()..@'`.",
            numbers(pull_requests),
        )
    })
}

/// What `close` says about the pull requests it took out of a stack and left out
/// of one, or `None` where it left none.
///
/// The sibling of [`left_unstacked_by_a_land`], and it differs by more than the
/// command word — the land's remedy is not merely inapplicable here but wrong
/// twice over, and both halves are load-bearing:
///
/// - **The closed change is still in the chain.** `close` takes the pull request
///   number off the local change and leaves the change where it was, so
///   `jj spr diff --all -r 'trunk()..@'` opens a *new* pull request for it and
///   undoes the close that was asked for.
/// - **Skipping it in the revset does not work either.** A run does not step
///   over a change; it builds around it. A change is only chained onto the one
///   below when that one is its local parent (`diff::linear_base`), so what sat
///   on the closed change instead gets a base branch of its own carrying the
///   closed change's tree. Two consequences, and the sentence has to name the
///   second because it is the one that undoes what `close` announced: the chain
///   breaks across the gap, so nothing below it is stacked with anything above
///   ([`chains`] cuts and carries on, so either side can still register on its
///   own — just not as one stack); and the closed change's work goes into that
///   base branch, so it drops back out of the diffs above, which is the opposite
///   of what the retargeting just told the user had happened.
///
/// So the remedy names the one thing that does work: take the closed change out
/// of the local chain first. There is no rebase to mention, unlike the land's —
/// a close puts nothing on the master branch, so nothing above it has moved.
///
/// Plural for the same reason as the land's: `close --all` closes several pull
/// requests in one run, and each of them can free a different stack.
pub fn left_unstacked_by_a_close(pull_requests: &[u64]) -> Option<String> {
    (!pull_requests.is_empty()).then(|| {
        format!(
            "GitHub stack: {} left unstacked by the stacks this close took apart. Only \
             `jj spr diff` puts a stack back, and only over changes it pushes as one unbroken \
             chain — so take what you closed out of the chain first, by abandoning it or \
             folding it into a neighbour, and push what is left. A closed change left in the \
             chain is not skipped but built around: the run gives whatever sits on it a base \
             branch carrying its work, so the chain breaks across it and the changes you \
             closed drop back out of the diffs above. Pushed instead, it simply opens a new \
             pull request.",
            numbers(pull_requests),
        )
    })
}

/// `stack #12` / `stacks #12, #13`.
fn stacks(stack_numbers: &[u64]) -> String {
    let plural = if stack_numbers.len() == 1 { "" } else { "s" };

    format!("stack{plural} {}", numbers(stack_numbers))
}

/// Decide what to do about the chain `desired`, given every open stack that
/// will still be standing and holds any of it, as `(stack number, that stack's
/// pull requests bottom to top)`.
///
/// Made offline and separately from the calls so that it can be read and tested
/// on its own: it is the only place that decides to *rebuild* a stack.
/// [`StackSession::unlock_base`] is the other thing that dissolves one, and it
/// has no decision to make — a base ref cannot move while a stack holds it.
///
/// **Every stack holding any member has to be accounted for, not just the one
/// holding the bottom.** GitHub refuses to create a stack out of a pull request
/// that is in one already, and refuses to add one, so a stack left out of this
/// answer is a 422 rather than a stack that quietly stays put.
///
/// With one stack in the way, the questions are asked cheapest first:
///
/// 1. **The stack already holds the chain**, as a run of adjacent members.
///    Nothing to do. This is not merely the equal case: a stack keeps its
///    merged and closed members forever, so GitHub can perfectly well hold
///    *more* than the run pushed with the chain sitting inside it — a member
///    closed unmerged, or one merged from GitHub's own interface, leaves the
///    stack exactly like that. Recreating for those would churn the stack's
///    number and URL on every push. (`jj spr land` does not produce this
///    state: it dissolves the stack rather than merging within it, so the run
///    after a land finds no stack at all and creates one.)
/// 2. **The chain continues the stack.** Whatever the stack ends with is the
///    bottom of the chain, and the rest is new, so it can be appended — the one
///    thing the API can do to a stack that exists.
/// 3. **Anything else**, which is a reorder, a removal, or a chain that reaches
///    below what the stack holds. None of those can be expressed, so the stack
///    is dissolved and rebuilt.
///
/// A stack holding a pull request whose base the run moves does not get as far
/// as any of this: it cannot survive the retargeting whatever else is true, so
/// its holder is taken out before `plan` is asked. That is why `plan` speaks
/// only of the stacks that will still be standing.
fn plan(desired: &[u64], holders: &[(u64, &[u64])]) -> Reconciliation {
    if desired.len() < 2 {
        return Reconciliation::NotAStack;
    }

    let dissolve_all = || Reconciliation::Recreate {
        dissolved: {
            let mut numbers: Vec<u64> = holders.iter().map(|(number, _)| *number).collect();
            numbers.sort_unstable();
            numbers
        },
        stack_number: None,
    };

    let [(stack_number, held)] = holders else {
        return if holders.is_empty() {
            Reconciliation::Create { stack_number: None }
        } else {
            // More than one stack is in the way, and there is no arrangement of
            // append and create that leaves one stack holding a chain spread
            // across several. They all go.
            dissolve_all()
        };
    };

    // A stack cannot really be empty — `POST /stacks` takes at least two pull
    // requests — so an empty member list means the key was absent or unreadable
    // and `#[serde(default)]` filled it in. That is "we did not learn what this
    // holds", not "it holds nothing", and dissolving a stack is not undoable:
    // leave it alone and say nothing changed.
    if held.is_empty() {
        return Reconciliation::UpToDate {
            stack_number: *stack_number,
        };
    }

    if held.windows(desired.len()).any(|window| window == desired) {
        return Reconciliation::UpToDate {
            stack_number: *stack_number,
        };
    }

    if let Some(addition) = appendable(desired, held) {
        return Reconciliation::Append {
            stack_number: *stack_number,
            pull_requests: addition,
        };
    }

    dissolve_all()
}

/// The tail of `desired` that could be added to a stack currently holding
/// `held`, or `None` where appending would not leave the stack holding the
/// chain.
///
/// `/add` puts pull requests on the *top* of a stack, so this only works out
/// when the stack already ends with the bottom of the chain. The rest of the
/// chain must be new: a pull request that is in the stack already is one the
/// API refuses to add, and one sitting lower down would mean a reorder rather
/// than an append.
fn appendable(desired: &[u64], held: &[u64]) -> Option<Vec<u64>> {
    // Longest first, so that as little as possible is claimed to be new.
    for overlap in (1..desired.len()).rev() {
        if !held.ends_with(&desired[..overlap]) {
            continue;
        }

        let addition = &desired[overlap..];
        if addition.iter().any(|number| held.contains(number)) {
            return None;
        }

        return Some(addition.to_vec());
    }

    None
}

/// Whether the run moving a base ref means this stack has to come apart.
///
/// GitHub refuses to change the base of a pull request while a stack holds it,
/// so a stack holding one the run retargets cannot be left standing, whatever
/// shape it is otherwise in.
fn holds_a_retargeted_pull_request(stack: &Stack, retargeted: &HashSet<u64>) -> bool {
    stack
        .pull_requests
        .iter()
        .any(|pull_request| retargeted.contains(&pull_request.number))
}

/// The stacks that come apart, and whether the chain ends up in a stack.
///
/// This is the whole of the bookkeeping behind
/// [`StackSession::orphaned_pull_requests`], and it is kept a plain function of
/// the plan and the stacks so that it can be read and tested on its own: a real
/// run learns these facts from the calls it makes, a dry run makes none of
/// them, and the two have to agree. Every review of this change so far has found
/// them not agreeing, each time somewhere different, so the two now come from
/// here.
///
/// `doomed` always comes apart — the run is moving a base ref of something it
/// holds — and on top of that only [`Reconciliation::Recreate`] takes anything
/// apart, naming what by number.
fn effects<'a>(
    plan: &Reconciliation,
    doomed: &'a [Stack],
    surviving: &'a [Stack],
) -> (Vec<&'a Stack>, bool) {
    let rebuilt: &[u64] = match plan {
        Reconciliation::Recreate { dissolved, .. } => dissolved,
        _ => &[],
    };

    let dissolved = doomed
        .iter()
        .chain(
            surviving
                .iter()
                .filter(|stack| rebuilt.contains(&stack.number)),
        )
        .collect();

    let registers = matches!(
        plan,
        Reconciliation::UpToDate { .. }
            | Reconciliation::Append { .. }
            | Reconciliation::Create { .. }
            | Reconciliation::Recreate { .. }
    );

    (dissolved, registers)
}

/// What became of a pull request asked to leave the stack it is in, so that
/// GitHub will accept something it refuses while a stack holds one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BaseUnlock {
    /// The pull request was in no stack, so nothing held its base ref.
    NotStacked,

    /// The stack was dissolved, releasing this pull request and every other
    /// member.
    Dissolved { stack_number: u64 },
}

/// The stack bookkeeping for one run of a command that touches stacks.
///
/// `diff` registers stacks and dissolves the ones in the way of a base it is
/// moving; `land` and `close` only ever dissolve. All three hold one as an
/// `Option`, which is `None` when GitHub is not drawing the stack. It remembers what
/// it has already asked GitHub so that a run does not ask twice: which pull
/// requests are known to be out of a stack, and whether the repository turned
/// out not to support stacks at all.
#[derive(Debug, Default)]
pub struct StackSession {
    /// Pull requests known to be in no stack, either because GitHub said so or
    /// because this run took them out of one. Sound only within a run: nothing
    /// stacks anything until the run's last step.
    released: HashSet<u64>,

    /// Set once GitHub answers that the repository has no stacked pull
    /// requests, so the rest of the run stops asking.
    unsupported: bool,

    /// What each stack this run took apart was holding when it did.
    ///
    /// Kept per stack rather than as a count or a flag because dissolving is
    /// not undoable and takes down pull requests the run may know nothing
    /// about: what has to be reported is which of *those* nothing put back,
    /// which cannot be worked out from the stack numbers alone.
    dissolved: Vec<Vec<u64>>,

    /// Every pull request this run has left in a stack.
    registered: HashSet<u64>,

    /// Pull requests this run finished with, by merging or closing them, which
    /// are therefore not something a dissolved stack lost.
    ///
    /// Such a pull request is out of every stack for good and cannot be put back
    /// into one: nothing but `diff` registers a stack, and `diff` registers only
    /// the pull requests of changes it pushes. That is the same reason a stack's
    /// merged members are left out of what a dissolve is reported to have lost.
    ///
    /// Both callers need it, for slightly different reasons. `land` dissolves
    /// before it merges, so its pull request is plainly an open member at the
    /// time. `close` dissolves *after* closing — and a closed-unmerged pull
    /// request is an [`unmerged_members`] member all the same, so it lands in
    /// the dissolved list too. Changing that predicate, which
    /// [`unmerged_members`] records as a known limitation, would make
    /// [`Self::closed`] redundant: the two are coupled and should move together.
    gone: HashSet<u64>,

    /// Stacks a dry run worked out that the run would take apart. Only a dry
    /// run fills this: a real run has already said so as it happened.
    coming_apart: Vec<u64>,
}

impl StackSession {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register every chain of `links` that GitHub could hold as a stack.
    ///
    /// `apply` is what tells a real run from a dry one: both look up what
    /// GitHub holds, because the outcome cannot be worked out without it, and
    /// only a real run acts on the answer.
    ///
    /// Always answers with at least one [`Reconciliation`], so that a run that
    /// registered nothing says why rather than silently doing nothing.
    ///
    /// Chains of one are walked too, rather than filtered out here: they can
    /// never become a stack, but a run that retargets one still takes apart
    /// whatever stack held it, and that has to be accounted for.
    pub async fn register(
        &mut self,
        gh: &impl GitHubApi,
        links: &[ChainLink],
        apply: bool,
    ) -> Result<Vec<Reconciliation>> {
        let chains = chains(links);

        if chains.is_empty() {
            return Ok(vec![Reconciliation::NotAStack]);
        }

        // The whole run's retargeted pull requests, not each chain's: a stack
        // can hold members that this run split across two chains, and moving
        // any one of their bases takes the whole stack apart.
        let retargeted: HashSet<u64> = chains
            .iter()
            .flat_map(|chain| chain.retargeted.iter().copied())
            .collect();

        let mut outcomes = Vec::with_capacity(chains.len());
        for chain in &chains {
            let outcome = self.register_chain(gh, chain, &retargeted, apply).await?;

            // Nothing more is going to work, and saying so once per chain would
            // be saying the same thing about the repository several times.
            if outcome == Reconciliation::Unsupported {
                return Ok(vec![outcome]);
            }

            outcomes.push(outcome);
        }

        // A run of several chains can report `NotAStack` for each of them,
        // which reads as a list of nothings. One is the answer.
        if outcomes.iter().all(|o| *o == Reconciliation::NotAStack) {
            outcomes.truncate(1);
        }

        if !self.coming_apart.is_empty() {
            let mut stack_numbers = std::mem::take(&mut self.coming_apart);
            stack_numbers.sort_unstable();
            stack_numbers.dedup();

            outcomes.insert(0, Reconciliation::Dissolving { stack_numbers });
        }

        Ok(outcomes)
    }

    /// Record that pull request `number` has merged, so that a stack this run
    /// dissolved is not reported as having lost it.
    ///
    /// `land` dissolves the stack while the pull request it is about to land is
    /// still open, so it goes into the dissolved list like any other member.
    /// Without this, every land under `spr.stackDisplay = github` would end by telling
    /// the user to put the pull request it had just landed back into a stack.
    pub fn merged(&mut self, number: u64) {
        self.gone.insert(number);
    }

    /// Record that pull request `number` has been closed. The same bookkeeping
    /// as [`Self::merged`]; a separate name so that the call site says which of
    /// the two it did.
    pub fn closed(&mut self, number: u64) {
        self.gone.insert(number);
    }

    /// The pull requests this run took out of a stack and left out of one.
    ///
    /// Deliberately not part of [`Self::register`]'s answer: the run may fail
    /// before it ever registers, and that is precisely the case worth
    /// reporting, so the caller asks separately and asks whatever happened.
    pub fn orphaned(&self) -> Vec<u64> {
        let mut lost: Vec<u64> = self
            .dissolved
            .iter()
            .flatten()
            .copied()
            .filter(|number| !self.registered.contains(number) && !self.gone.contains(number))
            .collect();

        lost.sort_unstable();
        lost.dedup();

        lost
    }

    /// [`Self::orphaned`] as something for `diff` to report, or `None` where
    /// there is nothing to report.
    ///
    /// `diff`'s and no one else's: [`Reconciliation::Orphaned`] speaks of what a
    /// run pushed and registered, and `land` and `close` push and register
    /// nothing, so each words its own sentence around [`Self::orphaned`] —
    /// [`left_unstacked_by_a_land`] and [`left_unstacked_by_a_close`].
    pub fn orphaned_pull_requests(&self) -> Option<Reconciliation> {
        let lost = self.orphaned();

        (!lost.is_empty()).then_some(Reconciliation::Orphaned {
            pull_requests: lost,
        })
    }

    async fn register_chain(
        &mut self,
        gh: &impl GitHubApi,
        chain: &Chain,
        retargeted: &HashSet<u64>,
        apply: bool,
    ) -> Result<Reconciliation> {
        // Every member is asked about, not just the bottom. A stack can only be
        // appended to, so it can never grow downwards: a run whose chain
        // reaches below what a stack holds — or whose members ended up in
        // different stacks — has to dissolve those stacks, and a lookup that
        // asked only about the bottom would not have seen them.
        let mut holders: Vec<Stack> = Vec::new();
        for number in &chain.pull_requests {
            match self.open_stack_for(gh, *number).await? {
                Lookup::Unsupported => return Ok(Reconciliation::Unsupported),
                Lookup::Unstacked => (),
                Lookup::Stacked(stack) => {
                    if !holders.iter().any(|held| held.number == stack.number) {
                        holders.push(stack);
                    }
                }
            }
        }

        // A stack holding a pull request whose base the run moves cannot be left
        // standing, whatever else is true of it, so it is taken out of the
        // question before the question is asked. `retargeted` is the whole run's
        // set rather than this chain's: the pull request that dooms a stack may
        // have ended up in a different chain from the other members of it.
        //
        // On a real run these stacks are already gone — the loop dissolved them
        // as it retargeted — so this finds nothing and only a dry run reaches
        // it. That is the point: without it a dry run would report the stack it
        // can still see instead of what the run would do to it.
        let (doomed, surviving): (Vec<Stack>, Vec<Stack>) = holders
            .into_iter()
            .partition(|stack| holds_a_retargeted_pull_request(stack, retargeted));

        let held: Vec<(u64, Vec<u64>)> = surviving
            .iter()
            .map(|stack| (stack.number, stack.pull_request_numbers()))
            .collect();
        let plan = plan(
            &chain.pull_requests,
            &held
                .iter()
                .map(|(number, members)| (*number, &members[..]))
                .collect::<Vec<_>>(),
        );

        if !apply {
            // A dry run makes none of the calls that record what becomes of each
            // pull request, so it records the same facts here instead. That is
            // what makes the reports at the end of the run read the same either
            // way rather than being worked out twice from different things.
            let (dissolved, registers) = effects(&plan, &doomed, &surviving);

            for stack in dissolved {
                self.dissolved.push(unmerged_members(stack));
            }

            if registers {
                self.registered.extend(&chain.pull_requests);
            }

            // Only the doomed stacks: what a `Recreate` takes apart it names
            // itself, whereas these are taken apart by the retargeting in the
            // loop and no chain's answer would otherwise mention them.
            self.coming_apart
                .extend(doomed.iter().map(|stack| stack.number));

            return Ok(plan);
        }

        match plan {
            Reconciliation::Append {
                stack_number,
                pull_requests,
            } => {
                propagate(gh.add_to_stack(stack_number, &pull_requests).await)?;
                self.registered.extend(&chain.pull_requests);

                Ok(Reconciliation::Append {
                    stack_number,
                    pull_requests,
                })
            }
            Reconciliation::UpToDate { stack_number } => {
                self.registered.extend(&chain.pull_requests);

                Ok(Reconciliation::UpToDate { stack_number })
            }
            Reconciliation::Create { .. } => Ok(Reconciliation::Create {
                stack_number: Some(self.create(gh, &chain.pull_requests).await?),
            }),
            Reconciliation::Recreate { dissolved, .. } => {
                for stack_number in &dissolved {
                    // Looked up rather than zipped: `dissolved` is sorted by
                    // stack number and `surviving` is in the order the members
                    // were asked about, so the two do not line up.
                    let members = surviving
                        .iter()
                        .find(|stack| stack.number == *stack_number)
                        .map(unmerged_members)
                        .unwrap_or_default();

                    self.dissolve(gh, *stack_number, &members).await?;
                }

                Ok(Reconciliation::Recreate {
                    dissolved,
                    stack_number: Some(self.create(gh, &chain.pull_requests).await?),
                })
            }
            unchanged => Ok(unchanged),
        }
    }

    /// Take pull request `number` out of the stack it is in, so that GitHub
    /// will accept something it refuses while a stack holds one — a change to
    /// its base ref, or an ordinary merge of it.
    ///
    /// The base ref is the case with a subtlety. GitHub refuses any `PATCH` to
    /// a stacked pull request that carries a `base` field — the same 422
    /// whether the value differs or not — so this has to happen before a run
    /// retargets one rather than after it fails.
    /// Asking first also keeps the decision on facts this side of the network:
    /// `diff` knows exactly where it is about to send a base, whereas telling
    /// that refusal apart from every other 422 means matching GitHub's prose,
    /// which has been reworded once already.
    ///
    /// Dissolving a stack releases every member, so a run pays one lookup per
    /// pull request it has not already released this way and one unstack per
    /// stack, however many pull requests it retargets. It takes down the
    /// *whole* stack, including pull requests this run knows nothing about. Only the ones the run pushes can be registered again, under a new
    /// number: there is no way to change a base and keep the stack, and none to
    /// put a stack back as it was. Whatever the run does not put back has to be
    /// said out loud; see [`Self::orphaned_pull_requests`].
    pub async fn unlock_base(&mut self, gh: &impl GitHubApi, number: u64) -> Result<BaseUnlock> {
        if self.released.contains(&number) {
            return Ok(BaseUnlock::NotStacked);
        }

        let stack = match self.open_stack_for(gh, number).await? {
            // Nothing can be holding the base ref if the repository has no
            // stacks at all.
            Lookup::Unsupported | Lookup::Unstacked => {
                self.released.insert(number);
                return Ok(BaseUnlock::NotStacked);
            }
            Lookup::Stacked(stack) => stack,
        };

        // Released covers every member — a merged one is no longer in a stack
        // either — while what is recorded as lost is only the members that
        // could be put back into one.
        self.dissolve(gh, stack.number, &unmerged_members(&stack))
            .await?;
        self.released.extend(stack.pull_request_numbers());

        Ok(BaseUnlock::Dissolved {
            stack_number: stack.number,
        })
    }

    /// The open stack holding pull request `number`.
    async fn open_stack_for(&mut self, gh: &impl GitHubApi, number: u64) -> Result<Lookup> {
        if self.unsupported {
            return Ok(Lookup::Unsupported);
        }

        // Deliberately the open-stack lookup: the unfiltered one goes on
        // answering with the closed stack a merged pull request was merged in,
        // for ever. See `GitHub::get_stacks_for_pull_request`.
        match self.degrade(gh.get_open_stack_for_pull_request(number).await)? {
            None => Ok(Lookup::Unsupported),
            Some(None) => Ok(Lookup::Unstacked),
            Some(Some(stack)) => Ok(Lookup::Stacked(stack)),
        }
    }

    /// Make a stack of `chain`, answering with its number.
    async fn create(&mut self, gh: &impl GitHubApi, chain: &[u64]) -> Result<u64> {
        let stack = propagate(gh.create_stack(chain).await)?;
        self.registered.extend(chain);

        Ok(stack.number)
    }

    /// Dissolve stack `stack_number`, which is holding `members`.
    async fn dissolve(
        &mut self,
        gh: &impl GitHubApi,
        stack_number: u64,
        members: &[u64],
    ) -> Result<()> {
        let outcome = propagate(gh.unstack(stack_number).await)?;

        match outcome {
            UnstackOutcome::Dissolved => {
                self.dissolved.push(members.to_vec());

                Ok(())
            }
            // The stack came back, holding what GitHub would not release.
            // Merged members are kept as history and are nothing to act on; a
            // member left behind that is *not* merged still holds the lock on
            // its base ref, so going on would fail at the next base change with
            // a message about stacks that says nothing about why.
            UnstackOutcome::Retained(stack) => {
                let stuck = unmerged_members(&stack);

                // What did come out is out, and has to be accounted for even
                // though this call is about to fail. What did *not* must not be
                // reported as left unstacked: it is still in the stack, and
                // telling the user to put it back would contradict the error
                // below in the same breath.
                self.dissolved.push(
                    members
                        .iter()
                        .filter(|number| !stuck.contains(number))
                        .copied()
                        .collect(),
                );

                if stuck.is_empty() {
                    return Ok(());
                }

                Err(Error::new(format!(
                    "GitHub would not take {} out of stack #{stack_number}. That is what it \
                     does for a pull request that is queued for merge or has auto-merge \
                     enabled. Wait for it to land, or turn auto-merge off, and run this again.",
                    numbers(&stuck),
                )))
            }
        }
    }

    /// Report a stacks failure, unless it says only that the repository does
    /// not have stacked pull requests enabled — which is a reason to do less
    /// than was asked, not to fail.
    fn degrade<T>(&mut self, result: StackResult<T>) -> Result<Option<T>> {
        match result {
            Ok(value) => Ok(Some(value)),
            Err(StackApiError::NotEnabled) => {
                self.unsupported = true;
                Ok(None)
            }
            Err(error) => Err(Error::new(error.to_string())),
        }
    }
}

/// Report a stacks failure as an ordinary error.
///
/// Unlike [`StackSession::degrade`] this is for a call made once the repository
/// is known to have stacks, where a 404 would say something else entirely and
/// there is nothing to degrade to. It keeps no state, which is the whole
/// difference between the two.
fn propagate<T>(result: StackResult<T>) -> Result<T> {
    result.map_err(|error| Error::new(error.to_string()))
}

/// The answer to "which open stack is this pull request in?".
enum Lookup {
    /// The repository has not opted in to stacked pull requests.
    Unsupported,
    /// The pull request is in no open stack.
    Unstacked,
    Stacked(Stack),
}

/// The stack's members that are not merged.
///
/// The distinction carries two jobs, and merged is the right test for both
/// because merged is the state GitHub was observed to keep in a stack come what
/// may — a merged pull request stays in its stack for ever, and stays even
/// through an unstack:
///
/// - **What an unstack left behind.** A stack that comes back holding nothing
///   but merged members has released everything it could; one holding anything
///   else is holding a base ref it would not let go of. The status code does
///   not draw that line, so this does.
/// - **What a dissolved stack lost.** A merged member cannot be put back into a
///   stack and does not need to be, so reporting it as left behind would be
///   telling the user to fix something that is neither broken nor fixable.
///
/// **Known limitation, second job only.** A *closed* member is as unrecoverable
/// as a merged one — `diff` registers only the pull requests of the changes it
/// pushes, and a closed pull request has none — but it is counted as lost all
/// the same. The run that does the closing covers itself: `close` records the
/// pull request with [`StackSession::closed`], as `land` does with
/// [`StackSession::merged`]. What is not covered is a member closed by an
/// *earlier* run. `close` dissolves the stacks holding the pull requests it is
/// about to *move*, not the one holding the pull request it closes, so a closed
/// member is stranded in an open stack whenever that stack survives the close —
/// because nothing was stacked on it, or because what was is in a different
/// stack or in none. That is the ordinary state, not a corner. A later dissolve
/// of the surviving stack then names the closed member, and the user is told to
/// push back a change whose pull request they closed.
///
/// The fix is a second predicate here — open rather than merely unmerged — for
/// the second job only, since a closed member left behind by an unstack
/// genuinely does still hold its base ref and so must go on counting as stuck
/// for the first. Left for its own change: it reaches `diff`'s reporting as
/// well, and it would make [`StackSession::closed`] redundant for the closing
/// run, which is a decision worth taking on its own.
fn unmerged_members(stack: &Stack) -> Vec<u64> {
    stack
        .pull_requests
        .iter()
        .filter(|pull_request| !pull_request.is_merged())
        .map(|pull_request| pull_request.number)
        .collect()
}

/// Why a stack is being dissolved, as something to tell the user.
///
/// An enum rather than the sentence itself so that every word jj-spr says about
/// dissolving a stack is written here, beside [`Reconciliation::describe`], in
/// one voice. The callers are two command modules, and a `&str` parameter would
/// have each of them keeping its own copy of prose about a subject neither owns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DissolveReason {
    /// A base ref is about to move, and a stack owns its members' base refs.
    ToMoveABase,

    /// A pull request the stack holds is being landed.
    ToLand,

    /// The pull request below this one has landed, so this one belongs on the
    /// master branch now.
    ToFollowALanding,

    /// The pull request below this one has been closed, so this one belongs on
    /// *that* pull request's base now.
    ///
    /// Not [`Self::ToFollowALanding`]: where the pull request above ends up is
    /// the whole difference — the master branch after a land, the closed pull
    /// request's own base after a close.
    ToFollowAClosing,
}

impl DissolveReason {
    /// A clause completing "Dissolved GitHub stack #N: ...".
    fn describe(self) -> &'static str {
        match self {
            Self::ToMoveABase => {
                "a stacked pull request's base cannot be changed. A stack is registered again \
                 at the end of this run, under a new number."
            }
            Self::ToLand => {
                "a pull request a stack holds cannot be merged on its own. GitHub's ordinary \
                 merge refuses it outright, and its stack merge lands every pull request \
                 below this one as well and then rebases the one above onto the master \
                 branch, closing it. So the stack comes apart and this pull request is merged \
                 by itself. Run `jj spr diff` afterwards to register what is left as a stack \
                 again, under a new number."
            }
            Self::ToFollowALanding => {
                "a stacked pull request's base cannot be changed, and this one is being \
                 pointed at the master branch now that the pull request below it has landed. \
                 Run `jj spr diff` to register what is left as a stack again, under a new \
                 number."
            }
            Self::ToFollowAClosing => {
                "a stacked pull request's base cannot be changed, and this one is being \
                 pointed at the base of the pull request below it, which has just been \
                 closed. Run `jj spr diff` to register what is left as a stack again, under a \
                 new number."
            }
        }
    }
}

/// Dissolve any stack holding pull request `number`, releasing every member.
///
/// A no-op when GitHub is not drawing the stack, which is what makes this safe to
/// call wherever a stack would be in the way without asking what mode the run
/// is in.
///
/// Named for what it does rather than for what the caller wanted, because the
/// two are not the same size: there is no way to take one pull request out of a
/// stack, so this releases every member — including pull requests the caller
/// knows nothing about and cannot put back. See [`StackSession::unlock_base`],
/// and [`StackSession::orphaned`] for what nothing put back — with
/// [`left_unstacked_by_a_land`], [`left_unstacked_by_a_close`] and
/// [`StackSession::orphaned_pull_requests`] as `land`'s, `close`'s and `diff`'s
/// ways of saying it.
pub async fn dissolve_any_stack_holding(
    stacks: Option<&mut StackSession>,
    gh: &impl GitHubApi,
    number: u64,
    why: DissolveReason,
) -> Result<()> {
    let Some(session) = stacks else {
        return Ok(());
    };

    if let BaseUnlock::Dissolved { stack_number } = session.unlock_base(gh, number).await? {
        output(
            "🧱",
            &format!("Dissolved GitHub stack #{stack_number}: {}", why.describe()),
        )?;
    }

    Ok(())
}

/// Dissolve the stacks holding the pull requests that are about to be retargeted
/// off one that is being landed or closed, reporting rather than raising a
/// failure.
///
/// [`crate::stacked::retarget_stacked_pull_requests`]' companion: a stack owns
/// its members' base refs, so every pull request that call is about to move has
/// to leave whatever stack holds it first. Usually one dissolve covers them all
/// — they were in one stack together with the pull request leaving it — and what
/// the rest of the loop catches is a pull request that ended up in a *different*
/// stack, which is the ordinary state after pushing part of a stack.
///
/// Kept here rather than beside its companion because [`crate::stacked`] holds
/// no stack knowledge at all: it is the retargeting both strategies share, it
/// imports nothing from this module, and it has never heard of a
/// [`StackSession`], a stack number, or `spr.stackDisplay = github`. Moving this there
/// would drag all of that into the one module that is deliberately free of it.
///
/// **Reported, not raised.** Both callers reach this having already done the
/// irreversible thing they came for — `land` has merged, `close` has closed and
/// stripped the pull request number off the local change — so a failure here has
/// nothing left to retry with, and raising would skip the retargeting of every
/// *other* pull request as well and, in `close --all`, stop the changes above
/// from being closed at all. A pull request left in a stack simply fails its own
/// retarget, which is what keeps the head branch of the one leaving alive for
/// it; see [`crate::stacked::Retargeted::may_delete_head_branch`]. The `Result`
/// is the terminal write's, not the stacks call's: no stacks failure leaves this
/// function.
pub async fn dissolve_stacks_holding(
    mut stacks: Option<&mut StackSession>,
    gh: &impl GitHubApi,
    pull_requests: &[StackedPullRequest],
    why: DissolveReason,
) -> Result<()> {
    for pull_request in pull_requests {
        if let Err(error) =
            dissolve_any_stack_holding(stacks.as_deref_mut(), gh, pull_request.number, why).await
        {
            output(
                "⚠️",
                &format!(
                    "Could not take Pull Request #{} out of its GitHub stack",
                    pull_request.number
                ),
            )?;
            for message in error.messages() {
                output("  ", message)?;
            }
        }
    }

    Ok(())
}

/// Tests for the stack registration.
///
/// Everything here is the decision-making, which is deliberately all the
/// library holds: the calls themselves are one line each and are exercised
/// end-to-end against GitHub by `spr/tests/github_e2e_test.rs`.
#[cfg(test)]
mod tests {
    use super::*;

    /// A change with a pull request that is chained to the one below it.
    fn on(number: u64, below: u64) -> ChainLink {
        ChainLink {
            pull_request: Some(number),
            based_on: Some(below),
            retargeted: false,
        }
    }

    /// A change with a pull request that starts a chain: the bottom of a run,
    /// or one whose base is not the branch of anything the run pushed.
    fn bottom(number: u64) -> ChainLink {
        ChainLink {
            pull_request: Some(number),
            based_on: None,
            retargeted: false,
        }
    }

    /// A change whose pull request does not exist yet.
    fn unopened() -> ChainLink {
        ChainLink {
            pull_request: None,
            based_on: None,
            retargeted: false,
        }
    }

    /// The pull requests of each chain, which is what most of these tests are
    /// about; `retargeted` has tests of its own.
    fn numbers_of(chains: Vec<Chain>) -> Vec<Vec<u64>> {
        chains.into_iter().map(|c| c.pull_requests).collect()
    }

    /// A chain of `pull_requests` that the run does not retarget.
    fn chain(pull_requests: &[u64]) -> Chain {
        Chain {
            pull_requests: pull_requests.to_vec(),
            retargeted: Vec::new(),
        }
    }

    /// A chain of `pull_requests` whose `retargeted` members the run moves onto
    /// a different base branch.
    fn retargeting(pull_requests: &[u64], retargeted: &[u64]) -> Chain {
        Chain {
            pull_requests: pull_requests.to_vec(),
            retargeted: retargeted.to_vec(),
        }
    }

    /// A change whose pull request is chained to the one below and whose base
    /// this run moves.
    fn moved(number: u64, below: u64) -> ChainLink {
        ChainLink {
            retargeted: true,
            ..on(number, below)
        }
    }

    #[test]
    fn a_chain_is_the_run_read_bottom_up() {
        assert_eq!(
            numbers_of(chains(&[bottom(73), on(74, 73), on(75, 74)])),
            vec![vec![73, 74, 75]]
        );
    }

    /// The bottom of a run is not chained to anything — nothing below it was
    /// pushed — so a chain has to be able to start at a link that says so, or
    /// no run would ever produce one.
    #[test]
    fn a_chain_starts_at_a_link_that_is_based_on_nothing() {
        assert_eq!(numbers_of(chains(&[bottom(73)])), vec![vec![73]]);
    }

    /// The break the whole accumulator exists for: a change whose pull request
    /// is not based on the one below it is where GitHub's chain rule stops
    /// holding, and a stack spanning it would be refused.
    #[test]
    fn a_link_based_on_nothing_starts_a_new_chain() {
        assert_eq!(
            numbers_of(chains(&[bottom(73), on(74, 73), bottom(75), on(76, 75)])),
            vec![vec![73, 74], vec![75, 76]]
        );
    }

    /// A change that needs no push still reports its pull request, so a change
    /// in the middle of a stack that nobody touched does not sever it.
    #[test]
    fn a_change_that_needed_no_push_keeps_the_chain_together() {
        // The middle change is indistinguishable here from one that was
        // pushed, which is the point: what it reports is the same either way.
        assert_eq!(
            numbers_of(chains(&[bottom(73), on(74, 73), on(75, 74)])),
            vec![vec![73, 74, 75]]
        );
    }

    /// A pull request that does not exist yet cannot be named as anything's
    /// base, so nothing above it can be chained to it.
    #[test]
    fn a_change_without_a_pull_request_breaks_the_chain() {
        assert_eq!(
            numbers_of(chains(&[bottom(73), on(74, 73), unopened(), bottom(76)])),
            vec![vec![73, 74], vec![76]]
        );
    }

    /// A run given revisions that do not form one chain hands over links whose
    /// base is a pull request that is not the one before it.
    #[test]
    fn a_link_to_something_other_than_the_change_below_starts_a_new_chain() {
        assert_eq!(
            numbers_of(chains(&[bottom(73), on(74, 73), on(76, 75)])),
            vec![vec![73, 74], vec![76]]
        );
    }

    #[test]
    fn a_run_with_no_pull_requests_has_no_chains() {
        assert!(numbers_of(chains(&[])).is_empty());
        assert!(numbers_of(chains(&[unopened(), unopened()])).is_empty());
    }

    /// GitHub has no such thing as a stack of one, and the schema refuses
    /// fewer than two outright.
    #[test]
    fn one_pull_request_is_not_a_stack() {
        assert_eq!(plan(&[73], &[]), Reconciliation::NotAStack);
        assert_eq!(plan(&[], &[]), Reconciliation::NotAStack);
        // Not even when a stack holds it: still not a stack to register.
        assert_eq!(plan(&[73], &[(76, &[73, 74])]), Reconciliation::NotAStack);
    }

    #[test]
    fn a_chain_no_stack_holds_gets_a_new_one() {
        assert_eq!(
            plan(&[73, 74], &[]),
            Reconciliation::Create { stack_number: None }
        );
    }

    #[test]
    fn a_chain_a_stack_already_holds_is_left_alone() {
        assert_eq!(
            plan(&[73, 74], &[(76, &[73, 74])]),
            Reconciliation::UpToDate { stack_number: 76 }
        );
    }

    /// The ordinary state after landing the bottom of a stack: GitHub keeps the
    /// merged pull request in the stack forever, so what it holds is a superset
    /// of what the run pushes from then on. Rebuilding for that would mint a
    /// new stack number, and a new URL, on every push.
    #[test]
    fn a_chain_inside_a_longer_stack_is_left_alone() {
        assert_eq!(
            plan(&[74, 75], &[(76, &[73, 74, 75])]),
            Reconciliation::UpToDate { stack_number: 76 }
        );
        assert_eq!(
            plan(&[74, 75], &[(76, &[73, 74, 75, 77])]),
            Reconciliation::UpToDate { stack_number: 76 }
        );
    }

    /// A new change on top of a stack is the one restructuring the API can do.
    #[test]
    fn a_chain_that_continues_a_stack_is_appended_to_it() {
        assert_eq!(
            plan(&[73, 74, 75], &[(76, &[73, 74])]),
            Reconciliation::Append {
                stack_number: 76,
                pull_requests: vec![75],
            }
        );
    }

    /// Only the part above what the stack ends with is added: adding a pull
    /// request the stack already holds is refused.
    #[test]
    fn only_the_new_pull_requests_are_appended() {
        assert_eq!(
            plan(&[74, 75, 77], &[(76, &[73, 74, 75])]),
            Reconciliation::Append {
                stack_number: 76,
                pull_requests: vec![77],
            }
        );
    }

    /// `/add` puts pull requests on top, so a chain that continues from the
    /// middle of a stack rather than from its top cannot be appended.
    #[test]
    fn a_chain_continuing_from_the_middle_of_a_stack_is_rebuilt() {
        assert_eq!(
            plan(&[73, 77], &[(76, &[73, 74])]),
            Reconciliation::Recreate {
                dissolved: vec![76],
                stack_number: None,
            }
        );
    }

    /// Reordering is the case there is no API for at all.
    #[test]
    fn a_reordered_chain_is_rebuilt() {
        assert_eq!(
            plan(&[74, 73], &[(76, &[73, 74])]),
            Reconciliation::Recreate {
                dissolved: vec![76],
                stack_number: None,
            }
        );
    }

    /// Dropping a change out of the middle of a stack is a removal, and there
    /// is no removal short of dissolving the stack.
    #[test]
    fn a_chain_missing_a_member_of_the_stack_is_rebuilt() {
        assert_eq!(
            plan(&[73, 75], &[(76, &[73, 74, 75])]),
            Reconciliation::Recreate {
                dissolved: vec![76],
                stack_number: None,
            }
        );
    }

    /// A stack cannot grow downwards — `/add` only appends — so a run whose
    /// chain reaches below what a stack holds has to dissolve it. The stack
    /// would otherwise not be looked at at all: it does not hold the chain's
    /// bottom, and `POST /stacks` would then be refused for the members it
    /// does hold.
    #[test]
    fn a_chain_reaching_below_a_stack_is_rebuilt() {
        assert_eq!(
            plan(&[73, 74, 75], &[(76, &[74, 75])]),
            Reconciliation::Recreate {
                dissolved: vec![76],
                stack_number: None,
            }
        );
    }

    /// The chain's members ended up in different stacks — landing part of a
    /// stack and pushing the rest can do it. No arrangement of create and
    /// append leaves one stack holding all of them, so they all go.
    #[test]
    fn a_chain_spread_across_two_stacks_dissolves_both() {
        assert_eq!(
            plan(&[73, 74, 75, 77], &[(80, &[73, 74]), (76, &[75, 77])]),
            Reconciliation::Recreate {
                dissolved: vec![76, 80],
                stack_number: None,
            }
        );
    }

    /// A chain of one is never a stack, whatever holds it. What becomes of what
    /// holds it is decided before `plan` is asked; see
    /// [`holds_a_retargeted_pull_request`].
    #[test]
    fn a_chain_of_one_is_not_a_stack() {
        assert_eq!(
            plan(&[74], &[(76, &[73, 74, 75])]),
            Reconciliation::NotAStack
        );
    }

    /// A stack with `members`, as the API answers with one.
    fn stack_of(number: u64, members: &[u64]) -> Stack {
        let pull_requests: Vec<String> = members
            .iter()
            .map(|number| {
                format!(
                    r#"{{ "number": {number}, "state": "open", "merged_at": null,
                          "head": {{ "ref": "spr/{number}" }} }}"#
                )
            })
            .collect();

        serde_json::from_str(&format!(
            r#"{{ "id": {number}, "number": {number}, "base": {{ "ref": "main" }},
                  "open": true, "pull_requests": [{}] }}"#,
            pull_requests.join(",")
        ))
        .unwrap()
    }

    /// The rule that keeps a retargeting run from writing into a stack GitHub
    /// would refuse: a stack holding a pull request whose base moves comes
    /// apart, whatever shape it is otherwise in.
    #[test]
    fn a_stack_holding_a_pull_request_whose_base_moves_is_doomed() {
        let retargeted: HashSet<u64> = [75].into_iter().collect();

        assert!(holds_a_retargeted_pull_request(
            &stack_of(76, &[75, 77]),
            &retargeted
        ));
    }

    /// ...and one that holds none of them is not. A change joining a stack from
    /// outside is retargeted onto the branch below it while the stack below is
    /// untouched: dissolving that would destroy a stack which could simply be
    /// appended to, and mint a new number for nothing. This is the case a
    /// chain-wide flag got wrong.
    #[test]
    fn a_stack_holding_no_retargeted_pull_request_stands() {
        let retargeted: HashSet<u64> = [75].into_iter().collect();

        assert!(!holds_a_retargeted_pull_request(
            &stack_of(76, &[73, 74]),
            &retargeted
        ));
        // ...and a run that retargets nothing dooms nothing.
        assert!(!holds_a_retargeted_pull_request(
            &stack_of(76, &[73, 74]),
            &HashSet::new()
        ));
    }

    /// The bookkeeping a dry run has to do for itself. A stack the run is about
    /// to retarget out of comes apart whatever the plan says, and on top of
    /// that only a rebuild takes anything apart.
    #[test]
    fn a_dry_run_accounts_for_every_stack_that_comes_apart() {
        let doomed = vec![stack_of(90, &[91, 92])];
        let surviving = vec![stack_of(76, &[73, 74]), stack_of(80, &[75])];
        let dissolved = |plan, doomed: &[Stack]| -> Vec<u64> {
            effects(&plan, doomed, &surviving)
                .0
                .iter()
                .map(|stack| stack.number)
                .collect()
        };

        // A rebuild names what it takes apart; the doomed stack is on top of it.
        assert_eq!(
            dissolved(
                Reconciliation::Recreate {
                    dissolved: vec![76],
                    stack_number: None,
                },
                &doomed
            ),
            vec![90, 76]
        );
        // Everything else takes apart only what the retargeting does...
        assert_eq!(
            dissolved(Reconciliation::Create { stack_number: None }, &doomed),
            vec![90]
        );
        assert_eq!(dissolved(Reconciliation::NotAStack, &doomed), vec![90]);
        // ...and with nothing being retargeted, nothing at all.
        assert!(dissolved(Reconciliation::Create { stack_number: None }, &[]).is_empty());
        assert!(dissolved(Reconciliation::UpToDate { stack_number: 76 }, &[]).is_empty());
        assert!(dissolved(Reconciliation::NotAStack, &[]).is_empty());
    }

    /// Which plans leave the chain in a stack, which is the other half of the
    /// orphan accounting: a chain that ends up in a stack has lost nothing.
    #[test]
    fn every_plan_but_doing_nothing_leaves_the_chain_in_a_stack() {
        let registers = |plan| effects(&plan, &[], &[]).1;

        assert!(registers(Reconciliation::Create { stack_number: None }));
        assert!(registers(Reconciliation::UpToDate { stack_number: 76 }));
        assert!(registers(Reconciliation::Append {
            stack_number: 76,
            pull_requests: vec![75],
        }));
        assert!(registers(Reconciliation::Recreate {
            dissolved: vec![76],
            stack_number: None,
        }));

        assert!(!registers(Reconciliation::NotAStack));
        assert!(!registers(Reconciliation::Unsupported));
    }

    /// A merged member cannot be put back into a stack and does not need to be,
    /// so it must not be counted as something the run lost — otherwise every
    /// restructure after a land tells the user to recover a pull request that
    /// has already landed.
    #[test]
    fn a_merged_member_is_not_something_a_dissolved_stack_lost() {
        let stack: Stack = serde_json::from_str(
            r#"{ "id": 1, "number": 76, "base": { "ref": "main" }, "open": true,
                 "pull_requests": [
                   { "number": 73, "state": "closed", "merged_at": "2026-07-31T16:44:19Z",
                     "head": { "ref": "spr/a" } },
                   { "number": 74, "state": "open", "merged_at": null,
                     "head": { "ref": "spr/b" } }
                 ] }"#,
        )
        .unwrap();

        assert_eq!(unmerged_members(&stack), vec![74]);
    }

    /// A retargeted link marks the chain it is part of, and only that one.
    /// Getting this the wrong side of the cut either dissolves a stack for
    /// nothing or leaves one standing that the run is about to break.
    #[test]
    fn a_moved_pull_request_marks_its_own_chain() {
        let split = chains(&[bottom(73), on(74, 73), moved(76, 75), on(77, 76)]);

        assert_eq!(
            split,
            vec![chain(&[73, 74]), retargeting(&[76, 77], &[76]),],
            "the move belongs to the chain it starts, not the one before it"
        );
    }

    /// ...and one in the middle of a chain marks that chain.
    #[test]
    fn a_moved_pull_request_in_the_middle_marks_its_chain() {
        assert_eq!(
            chains(&[bottom(73), moved(74, 73), on(75, 74)]),
            vec![retargeting(&[73, 74, 75], &[74])]
        );
    }

    /// A stack takes at least two pull requests, so no member list means the
    /// answer could not be read rather than that the stack is empty — and
    /// dissolving one is not undoable.
    #[test]
    fn a_stack_that_says_nothing_about_its_members_is_left_alone() {
        assert_eq!(
            plan(&[73, 74], &[(76, &[])]),
            Reconciliation::UpToDate { stack_number: 76 }
        );
    }

    /// An append must not be claimed when part of what would be added is
    /// already somewhere in the stack: GitHub refuses to add a pull request it
    /// already holds, so this has to be seen as the reorder it is.
    #[test]
    fn an_append_that_would_duplicate_a_member_is_rebuilt() {
        assert_eq!(
            appendable(&[74, 73], &[73, 74]),
            None,
            "appending #73 would put it in the stack twice"
        );
    }

    /// The longest overlap is the one taken, so as little as possible is
    /// claimed to be new.
    #[test]
    fn the_append_takes_the_longest_overlap() {
        assert_eq!(appendable(&[73, 74, 75], &[72, 73, 74]), Some(vec![75]));
    }

    #[test]
    fn a_disjoint_chain_cannot_be_appended() {
        assert_eq!(appendable(&[75, 77], &[73, 74]), None);
    }

    /// A stack that comes back from an unstack holding nothing but merged pull
    /// requests did everything it was asked to: merged members are kept as
    /// history and have no base ref left to lock.
    #[test]
    fn merged_members_left_behind_by_an_unstack_are_not_stuck() {
        let stack: Stack = serde_json::from_str(
            r#"{ "id": 1, "number": 76, "base": { "ref": "main" }, "open": false,
                 "pull_requests": [
                   { "number": 73, "state": "closed", "merged_at": "2026-07-31T16:44:19Z",
                     "head": { "ref": "spr/a" } }
                 ] }"#,
        )
        .unwrap();

        assert!(unmerged_members(&stack).is_empty());
    }

    /// An open member left behind is a pull request queued for merge or with
    /// auto-merge on. Its base ref is still locked, so the run cannot go on.
    #[test]
    fn an_open_member_left_behind_by_an_unstack_is_stuck() {
        let stack: Stack = serde_json::from_str(
            r#"{ "id": 1, "number": 76, "base": { "ref": "main" }, "open": true,
                 "pull_requests": [
                   { "number": 73, "state": "closed", "merged_at": "2026-07-31T16:44:19Z",
                     "head": { "ref": "spr/a" } },
                   { "number": 74, "state": "open", "merged_at": null,
                     "head": { "ref": "spr/b" } }
                 ] }"#,
        )
        .unwrap();

        assert_eq!(unmerged_members(&stack), vec![74]);
    }

    /// A run that pushed one change says nothing; one that could not register
    /// at all says so every time, because the setting is not doing what it
    /// says.
    #[test]
    fn only_the_outcomes_worth_reading_are_notable() {
        assert!(!Reconciliation::NotAStack.is_notable());
        assert!(!Reconciliation::UpToDate { stack_number: 76 }.is_notable());
        assert!(Reconciliation::Unsupported.is_notable());
        assert!(Reconciliation::Create { stack_number: None }.is_notable());
    }

    /// The same value describes a plan and the work done, so both have to read
    /// as a phrase, and the one that has a stack number has to name it.
    #[test]
    fn an_outcome_describes_itself_before_and_after_the_work() {
        assert_eq!(
            Reconciliation::Create {
                stack_number: Some(76)
            }
            .describe(),
            "a new stack, #76"
        );
        assert_eq!(
            Reconciliation::Create { stack_number: None }.describe(),
            "a new stack"
        );
        assert!(
            Reconciliation::Append {
                stack_number: 76,
                pull_requests: vec![74, 75],
            }
            .describe()
            .contains("#74, #75"),
        );
        assert_eq!(
            Reconciliation::Recreate {
                dissolved: vec![76],
                stack_number: Some(80),
            }
            .describe(),
            "stack #76 dissolved, and stack #80 in its place"
        );
        // Several stacks in the way read as several.
        assert_eq!(
            Reconciliation::Recreate {
                dissolved: vec![76, 80],
                stack_number: None,
            }
            .describe(),
            "stacks #76, #80 dissolved, and a new stack in its place"
        );
    }

    /// The dangerous silent case: a run took a stack apart to move a base and
    /// then registered nothing, so every pull request that was in it is loose.
    #[test]
    fn pull_requests_left_out_of_a_stack_are_reported() {
        let mut session = StackSession::new();
        session.dissolved = vec![vec![73, 74, 75]];

        assert_eq!(
            session.orphaned_pull_requests(),
            Some(Reconciliation::Orphaned {
                pull_requests: vec![73, 74, 75]
            })
        );
        assert!(
            session
                .orphaned_pull_requests()
                .expect("there is something to report")
                .is_notable(),
            "pull requests that went missing must not be reported only on a dry run"
        );
    }

    /// Only the ones nothing put back. A run that dissolved a stack and
    /// registered the same pull requests again has lost nothing — which is the
    /// ordinary whole-stack push, and must stay quiet.
    #[test]
    fn pull_requests_that_went_back_into_a_stack_are_not_reported() {
        let mut session = StackSession::new();
        session.dissolved = vec![vec![73, 74, 75]];
        session.registered = [73, 74].into_iter().collect();

        assert_eq!(
            session.orphaned_pull_requests(),
            Some(Reconciliation::Orphaned {
                pull_requests: vec![75]
            })
        );

        session.registered.insert(75);
        assert_eq!(session.orphaned_pull_requests(), None);

        assert_eq!(StackSession::new().orphaned_pull_requests(), None);
    }

    /// A dry run has to name the stacks the retargeting will take apart, even
    /// when every member goes back into a new one and nothing is reported as
    /// lost — the stack's number and URL do not survive, and a real run says so
    /// as it happens.
    #[test]
    fn a_dry_run_reports_the_stacks_it_would_take_apart() {
        assert_eq!(
            Reconciliation::Dissolving {
                stack_numbers: vec![76]
            }
            .describe(),
            "stack #76 taken apart first, because the run moves the base of a pull request in \
             it and GitHub does not allow that while a stack holds it. What this run pushes is \
             registered again as a new stack, with a new number"
        );
        assert!(
            Reconciliation::Dissolving {
                stack_numbers: vec![76]
            }
            .is_notable(),
            "a stack about to be destroyed must be reported"
        );
    }

    /// A pull request that has landed is out of every stack for good and cannot
    /// be put back into one, so the stack it was dissolved out of did not lose
    /// it. `land` dissolves while it is still open, so without this every land
    /// would end by telling the user to restack what it had just landed.
    #[test]
    fn a_pull_request_that_landed_is_not_something_the_stack_lost() {
        let mut session = StackSession::new();
        session.dissolved = vec![vec![73, 74, 75]];

        session.merged(74);

        assert_eq!(session.orphaned(), vec![73, 75]);
        assert_eq!(
            session.orphaned_pull_requests(),
            Some(Reconciliation::Orphaned {
                pull_requests: vec![73, 75]
            })
        );

        session.merged(73);
        session.merged(75);
        assert!(session.orphaned().is_empty());
        assert_eq!(session.orphaned_pull_requests(), None);
    }

    /// The same for a pull request that was closed. `close` dissolves *after*
    /// closing, and a closed-unmerged pull request is still an
    /// [`unmerged_members`] member, so it lands in the dissolved list like any
    /// other — without this every close under `spr.stackDisplay = github` would end by
    /// telling the user to put back into a stack the pull request they had just
    /// closed. `close --all` shows the rest of it: the run frees the whole stack
    /// on its first close and then closes the members one by one, so what is
    /// really loose is only what it did not reach.
    #[test]
    fn a_pull_request_that_was_closed_is_not_something_the_stack_lost() {
        let mut session = StackSession::new();
        session.dissolved = vec![vec![73, 74, 75]];

        session.closed(73);
        assert_eq!(session.orphaned(), vec![74, 75]);

        session.closed(74);
        session.closed(75);
        assert!(session.orphaned().is_empty());
    }

    /// Every sentence jj-spr says about dissolving a stack is written in one
    /// place, and each has to read as a clause completing "Dissolved GitHub
    /// stack #N: ...".
    #[test]
    fn every_reason_for_dissolving_a_stack_reads_as_a_clause() {
        for why in [
            DissolveReason::ToMoveABase,
            DissolveReason::ToLand,
            DissolveReason::ToFollowALanding,
            DissolveReason::ToFollowAClosing,
        ] {
            let clause = why.describe();

            assert!(
                clause.starts_with(char::is_lowercase),
                "{why:?} has to continue the sentence, not start one: {clause}"
            );
            assert!(
                clause.ends_with('.'),
                "{why:?} has to finish the sentence: {clause}"
            );
        }

        // The one that has to say what it costs: landing takes the stack away
        // for good, and `diff` is what puts one back.
        assert!(DissolveReason::ToLand.describe().contains("jj spr diff"));

        // The two "the one below it left" reasons must not read alike: where
        // the pull request above ends up is the whole difference between a land
        // and a close, and it is what the reader is being told.
        assert!(
            DissolveReason::ToFollowALanding
                .describe()
                .contains("master branch"),
        );
        assert!(
            DissolveReason::ToFollowAClosing
                .describe()
                .contains("base of the pull request below it"),
        );
    }

    /// Only `land` says this, and only about what its dissolving left behind —
    /// which can span more than one stack, and can never be put back as one.
    #[test]
    fn what_a_land_left_unstacked_is_reported_in_the_plural() {
        let sentence = left_unstacked_by_a_land(&[73, 75]).expect("there is something to report");

        assert!(sentence.contains("#73, #75"), "{sentence}");
        assert!(
            sentence.contains("stacks this land took apart"),
            "a land can take apart more than one stack: {sentence}"
        );
        assert!(
            sentence.contains("jj spr diff"),
            "nothing but `diff` puts a stack back: {sentence}"
        );

        assert_eq!(left_unstacked_by_a_land(&[]), None);
    }

    /// `close`'s version of the same report. It must not tell the reader to
    /// rebase: a close puts nothing on the master branch, so nothing above it
    /// has moved and there is only the push to make.
    #[test]
    fn what_a_close_left_unstacked_is_reported_without_a_rebase() {
        let sentence = left_unstacked_by_a_close(&[73, 75]).expect("there is something to report");

        assert!(sentence.contains("#73, #75"), "{sentence}");
        assert!(
            sentence.contains("stacks this close took apart"),
            "`close --all` can take apart more than one stack: {sentence}"
        );
        assert!(
            sentence.contains("jj spr diff"),
            "nothing but `diff` puts a stack back: {sentence}"
        );
        assert!(
            !sentence.contains("rebase"),
            "a close moves nothing, so there is nothing to rebase onto: {sentence}"
        );
        // Both halves of why the land's blanket revset is wrong here. Neither
        // is visible from this module, and a sentence that gave only one of
        // them would send the user down the other.
        assert!(
            sentence.contains("out of the chain"),
            "the closed change has to leave the local chain, or the chain breaks across it: \
             {sentence}"
        );
        assert!(
            sentence.contains("new pull request"),
            "a run that pushes the closed change opens a new pull request for it: {sentence}"
        );
        assert!(
            sentence.contains("drop back out of the diffs above"),
            "skipping the closed change is not free either — the run builds a base branch \
             around it: {sentence}"
        );

        assert_eq!(left_unstacked_by_a_close(&[]), None);
    }

    /// One chain registering must not silence what a different chain lost: the
    /// two have nothing to do with each other.
    #[test]
    fn one_chain_registering_does_not_cover_another_chains_losses() {
        let mut session = StackSession::new();
        session.dissolved = vec![vec![80, 81]];
        session.registered = [73, 74].into_iter().collect();

        assert_eq!(
            session.orphaned_pull_requests(),
            Some(Reconciliation::Orphaned {
                pull_requests: vec![80, 81]
            })
        );
    }
}
