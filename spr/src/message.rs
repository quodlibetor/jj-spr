/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use crate::{
    error::{Error, Result},
    output::output,
};

pub type MessageSectionsMap = std::collections::BTreeMap<MessageSection, String>;

#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Debug)]
pub enum MessageSection {
    Title,
    Summary,
    /// The PR stack this change belongs to. Unlike the other sections this one
    /// is generated rather than authored, and lives only in the PR body — it is
    /// left out of both the commit message and the merge message.
    Stack,
    Reviewers,
    ReviewedBy,
    PullRequest,
    CherryPick,
}

pub fn message_section_label(section: &MessageSection) -> &'static str {
    use MessageSection::*;

    match section {
        Title => "Title",
        Summary => "Summary",
        // Unreachable: `build_message` writes the stack section between markers
        // instead of labelling it. Kept so the match stays exhaustive, and to
        // name the label that older bodies still carry.
        Stack => "PR Stack",
        Reviewers => "Reviewers",
        ReviewedBy => "Reviewed By",
        PullRequest => "Pull Request",
        CherryPick => "Cherry Pick",
    }
}

pub fn message_section_by_label(label: &str) -> Option<MessageSection> {
    use MessageSection::*;

    match &label.to_ascii_lowercase()[..] {
        "title" => Some(Title),
        "summary" => Some(Summary),
        // Read-only compatibility: bodies labelled before the markers are
        // still on GitHub and must still parse. Nothing writes this label any
        // more — see STACK_BEGIN.
        //
        // A bare "stack" is deliberately not accepted: nothing ever wrote it,
        // and `Stack: React + Postgres` is ordinary prose in a commit message —
        // claiming it would delete the line, since `build_commit_message` omits
        // this section.
        "pr stack" => Some(Stack),
        "reviewer" => Some(Reviewers),
        "reviewers" => Some(Reviewers),
        "reviewed by" => Some(ReviewedBy),
        "pull request" => Some(PullRequest),
        "cherry pick" => Some(CherryPick),
        _ => None,
    }
}

/// Markers delimiting the generated [`MessageSection::Stack`] in a PR body.
///
/// They are HTML comments, so GitHub does not render them and nobody types one
/// by accident. They also free the section's text from the label grammar, which
/// admits only word characters and spaces, so the section can carry a rule and
/// a sentence.
///
/// This is what new bodies use. The `PR Stack:` label is still accepted on read
/// for bodies written before it — so a summary line shaped like that label is
/// still claimed by the section, which the markers do not change.
const STACK_BEGIN: &str = "<!-- spr-stack -->";
const STACK_END: &str = "<!-- /spr-stack -->";

/// Split the stack section out of a PR body, returning the rest of the body.
///
/// Takes the last *terminated* marker pair, searching back from the last
/// `STACK_END` for the `STACK_BEGIN` that opens it, and requiring each to own
/// its line. That is the one we wrote: [`build_github_body`] renders the
/// section last, so an earlier pair is someone quoting the format.
///
/// A marker with nothing closing it is ordinary text. It must not swallow the
/// rest of the body, which would take the `Pull Request:` line with it — and a
/// change whose PR link has vanished gets a second PR opened for it.
///
/// Known limitation: a body whose *only* marker pair is a quotation — someone
/// documenting this format, in a code fence say — has it taken as the section.
/// Telling the two apart needs a Markdown parser; the fence this replaced had
/// the same hazard.
fn split_stack_section(msg: &str) -> (String, Option<String>) {
    let Some(end) = rfind_line(msg, STACK_END) else {
        return (msg.to_owned(), None);
    };
    let Some(begin) = rfind_line(&msg[..end], STACK_BEGIN) else {
        return (msg.to_owned(), None);
    };

    let text = msg[begin + STACK_BEGIN.len()..end].trim().to_owned();
    let before = msg[..begin].trim();
    let rest = msg[end + STACK_END.len()..].trim();

    // Rejoin as separate paragraphs: run together with a single newline they
    // would render as one.
    let remainder = match (before.is_empty(), rest.is_empty()) {
        (true, _) => rest.to_owned(),
        (_, true) => before.to_owned(),
        _ => format!("{before}\n\n{rest}"),
    };

    (remainder, Some(text))
}

/// Byte offset of the last `marker` that is alone on its line.
///
/// A marker with text around it is prose mentioning the marker, not a
/// delimiter.
fn rfind_line(msg: &str, marker: &str) -> Option<usize> {
    msg.match_indices(marker)
        .filter(|(at, _)| {
            let before_is_clear = msg[..*at].chars().next_back().is_none_or(|c| c == '\n');
            let after_is_clear = msg[at + marker.len()..]
                .chars()
                .next()
                .is_none_or(|c| c == '\n');
            before_is_clear && after_is_clear
        })
        .map(|(at, _)| at)
        .last()
}

pub fn parse_message(msg: &str, top_section: MessageSection) -> MessageSectionsMap {
    let regex = lazy_regex::regex!(r#"^\s*([\w\s]+?)\s*:\s*(.*)$"#);

    // Only a PR body carries a stack section, so only a PR body is searched for
    // one. A commit message that merely quotes the markers — this feature's own
    // documentation, say — keeps its text.
    let (msg, stack) = if top_section == MessageSection::Summary {
        split_stack_section(msg)
    } else {
        (msg.to_owned(), None)
    };

    let mut section = top_section;
    let mut lines_in_section = Vec::<&str>::new();
    let mut sections = std::collections::BTreeMap::<MessageSection, String>::new();

    for (lineno, line) in msg
        .trim()
        .split('\n')
        .map(|line| line.trim_end())
        .enumerate()
    {
        if let Some(caps) = regex.captures(line) {
            let label = caps.get(1).unwrap().as_str();
            let payload = caps.get(2).unwrap().as_str();

            if let Some(new_section) = message_section_by_label(label) {
                append_to_message_section(
                    sections.entry(section),
                    lines_in_section.join("\n").trim(),
                );
                section = new_section;
                lines_in_section = vec![payload];
                continue;
            }
        }

        if lineno == 0 && top_section == MessageSection::Title {
            sections.insert(top_section, line.to_string());
            section = MessageSection::Summary;
        } else {
            lines_in_section.push(line);
        }
    }

    if !lines_in_section.is_empty() {
        append_to_message_section(sections.entry(section), lines_in_section.join("\n").trim());
    }

    if let Some(text) = stack {
        sections.insert(MessageSection::Stack, text);
    }

    sections
}

fn append_to_message_section(
    entry: std::collections::btree_map::Entry<MessageSection, String>,
    text: &str,
) {
    if !text.is_empty() {
        entry
            .and_modify(|value| {
                if value.is_empty() {
                    *value = text.to_string();
                } else {
                    *value = format!("{}\n\n{}", value, text);
                }
            })
            .or_insert_with(|| text.to_string());
    } else {
        entry.or_default();
    }
}

pub fn build_message(section_texts: &MessageSectionsMap, sections: &[MessageSection]) -> String {
    let mut result = String::new();
    let mut display_label = false;

    for section in sections {
        let value = section_texts.get(section);
        if let Some(text) = value {
            if !result.is_empty() {
                result.push('\n');
            }

            // The stack section is delimited by markers rather than a label —
            // see STACK_BEGIN. Its text is free-form, so it is written out
            // between them verbatim.
            if section == &MessageSection::Stack {
                result.push_str(&format!("{STACK_BEGIN}\n{text}\n{STACK_END}\n"));
                continue;
            }

            if section != &MessageSection::Title && section != &MessageSection::Summary {
                // Once we encounter a section that's neither Title nor Summary,
                // we start displaying the labels.
                display_label = true;
            }

            if display_label {
                let label = message_section_label(section);
                result.push_str(label);
                result.push_str(if label.len() + text.len() > 76 || text.contains('\n') {
                    ":\n"
                } else {
                    ": "
                });
            }

            result.push_str(text);
            result.push('\n');
        }
    }

    result.trim().to_owned()
}

/// Build the message for the local commit.
///
/// [`MessageSection::Stack`] is deliberately absent: it is generated for the PR
/// body, and `jj spr amend` writes these sections back to the commit, so
/// listing it here would put it in the user's commit message.
pub fn build_commit_message(section_texts: &MessageSectionsMap) -> String {
    build_message(
        section_texts,
        &[
            MessageSection::Title,
            MessageSection::Summary,
            MessageSection::Reviewers,
            MessageSection::ReviewedBy,
            MessageSection::PullRequest,
            MessageSection::CherryPick,
        ],
    )
}

/// Build the body for the PR.
///
/// `Stack` must stay last: [`split_stack_section`] reads the section back by
/// taking the final marker pair, so a section rendered after it would let prose
/// below the stack claim to be it.
pub fn build_github_body(section_texts: &MessageSectionsMap) -> String {
    build_message(
        section_texts,
        &[MessageSection::Summary, MessageSection::Stack],
    )
}

/// Render the `Stack` section body for the PR numbered `current`.
///
/// `stack` lists the stack's PR numbers bottom-up, the order the stack is built
/// in. It renders them top-down, the way a stack is drawn — `jj log` puts the
/// tip at the top — which is why it asks for review from the bottom up.
///
/// Returns `None` for a stack that does not need the section: one PR is not a
/// stack, and a stack we cannot place `current` in would render a list with no
/// "you are here" marker. The caller removes the section in that case.
pub fn build_stack_section(stack: &[u64], current: u64) -> Option<String> {
    if stack.len() < 2 || !stack.contains(&current) {
        return None;
    }

    let mut text = String::from("---\nPR stack, review from the bottom up:\n");
    for number in stack.iter().rev() {
        text.push_str(&format!("- #{number}"));
        if *number == current {
            text.push_str(" <- you are here");
        }
        text.push('\n');
    }

    Some(text.trim_end().to_owned())
}

/// Build the message for the squash-merge commit `jj spr land` creates.
///
/// [`MessageSection::Stack`] is deliberately absent, for the same reason as in
/// [`build_commit_message`]: the stack describes review, not history, and must
/// not outlive it.
pub fn build_github_body_for_merging(section_texts: &MessageSectionsMap) -> String {
    build_message(
        section_texts,
        &[
            MessageSection::Summary,
            MessageSection::Reviewers,
            MessageSection::ReviewedBy,
            MessageSection::PullRequest,
        ],
    )
}

pub fn validate_commit_message(message: &MessageSectionsMap) -> Result<()> {
    let title_missing_or_empty = match message.get(&MessageSection::Title) {
        None => true,
        Some(title) => title.is_empty(),
    };
    if title_missing_or_empty {
        output("💔", "Commit message does not have a title!")?;
        return Err(Error::empty());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    // Note this useful idiom: importing names from outer (for mod tests) scope.
    use super::*;

    #[test]
    fn test_parse_empty() {
        assert_eq!(
            parse_message("", MessageSection::Title),
            [(MessageSection::Title, "".to_string())].into()
        );
    }

    #[test]
    fn test_parse_title() {
        assert_eq!(
            parse_message("Hello", MessageSection::Title),
            [(MessageSection::Title, "Hello".to_string())].into()
        );
        assert_eq!(
            parse_message("Hello\n", MessageSection::Title),
            [(MessageSection::Title, "Hello".to_string())].into()
        );
        assert_eq!(
            parse_message("\n\nHello\n\n", MessageSection::Title),
            [(MessageSection::Title, "Hello".to_string())].into()
        );
    }

    #[test]
    fn test_parse_title_and_summary() {
        assert_eq!(
            parse_message("Hello\nFoo Bar", MessageSection::Title),
            [
                (MessageSection::Title, "Hello".to_string()),
                (MessageSection::Summary, "Foo Bar".to_string())
            ]
            .into()
        );
        assert_eq!(
            parse_message("Hello\n\nFoo Bar", MessageSection::Title),
            [
                (MessageSection::Title, "Hello".to_string()),
                (MessageSection::Summary, "Foo Bar".to_string())
            ]
            .into()
        );
        assert_eq!(
            parse_message("Hello\n\n\nFoo Bar", MessageSection::Title),
            [
                (MessageSection::Title, "Hello".to_string()),
                (MessageSection::Summary, "Foo Bar".to_string())
            ]
            .into()
        );
        assert_eq!(
            parse_message("Hello\n\nSummary:\nFoo Bar", MessageSection::Title),
            [
                (MessageSection::Title, "Hello".to_string()),
                (MessageSection::Summary, "Foo Bar".to_string())
            ]
            .into()
        );
    }

    #[test]
    fn test_parse_sections() {
        assert_eq!(
            parse_message(
                r#"Hello

Summary:
here is
the
summary

Reviewer:    a, b, c"#,
                MessageSection::Title
            ),
            [
                (MessageSection::Title, "Hello".to_string()),
                (MessageSection::Summary, "here is\nthe\nsummary".to_string()),
                (MessageSection::Reviewers, "a, b, c".to_string()),
            ]
            .into()
        );
    }

    const STACK_TEXT: &str = "\
---
PR stack, review from the bottom up:
- #123
- #122 <- you are here
- #121";

    fn stacked_sections() -> MessageSectionsMap {
        [
            (MessageSection::Title, "a title".to_string()),
            (MessageSection::Summary, "the summary".to_string()),
            (MessageSection::Stack, STACK_TEXT.to_string()),
            (
                MessageSection::PullRequest,
                "https://github.com/o/r/pull/122".to_string(),
            ),
        ]
        .into()
    }

    #[test]
    fn test_build_stack_section() {
        assert_eq!(
            build_stack_section(&[121, 122, 123], 122).as_deref(),
            Some(STACK_TEXT)
        );
    }

    #[test]
    fn test_build_stack_section_marks_the_ends_of_the_stack() {
        let bottom = build_stack_section(&[121, 122], 121).unwrap();
        assert!(bottom.contains("- #121 <- you are here"));
        assert!(bottom.contains("- #122\n"));

        let top = build_stack_section(&[121, 122], 122).unwrap();
        assert!(top.contains("- #122 <- you are here"));
        assert!(top.ends_with("- #121"));
    }

    /// A single PR is not a stack, so it gets no section.
    #[test]
    fn test_no_stack_section_for_a_lone_pull_request() {
        assert_eq!(build_stack_section(&[121], 121), None);
        assert_eq!(build_stack_section(&[], 121), None);
    }

    /// Without the current PR there would be no "you are here" to render.
    #[test]
    fn test_no_stack_section_when_current_is_not_in_the_stack() {
        assert_eq!(build_stack_section(&[121, 122], 999), None);
    }

    #[test]
    fn test_stack_section_is_rendered_into_the_github_body() {
        let body = build_github_body(&stacked_sections());

        assert!(body.starts_with("the summary"), "body was: {body}");
        assert!(body.contains("<!-- spr-stack -->\n"), "body was: {body}");
        assert!(body.contains("<!-- /spr-stack -->"), "body was: {body}");
        assert!(
            body.contains("PR stack, review from the bottom up:"),
            "body was: {body}"
        );
        assert!(body.contains("- #122 <- you are here"), "body was: {body}");
    }

    /// The section must never reach the local commit message, which is what
    /// `jj spr amend` writes there. This exercises the real function that
    /// `rewrite_commit_messages` uses.
    #[test]
    fn test_stack_section_is_not_in_the_commit_message() {
        let message = build_commit_message(&stacked_sections());

        assert!(!message.contains("Stack"), "message was: {message}");
        assert!(!message.contains("#122"), "message was: {message}");
        assert!(message.contains("the summary"));
        assert!(message.contains("Pull Request: https://github.com/o/r/pull/122"));
    }

    /// Nor the squash-merge message that `jj spr land` writes.
    #[test]
    fn test_stack_section_is_not_in_the_merge_message() {
        let message = build_github_body_for_merging(&stacked_sections());

        assert!(!message.contains("Stack"), "message was: {message}");
        assert!(!message.contains("#121"), "message was: {message}");
        assert!(message.contains("the summary"));
    }

    /// `Stack:` is ordinary prose in a commit message. Claiming it would delete
    /// the line, since `build_commit_message` omits this section — so only the
    /// label this actually renders is accepted.
    #[test]
    fn test_a_bare_stack_line_is_not_the_stack_section() {
        let msg = "feat: choose the stack\n\nStack: React + Postgres\n\nPull Request: https://x/1";

        let sections = parse_message(msg, MessageSection::Title);

        assert_eq!(sections.get(&MessageSection::Stack), None);
        assert!(
            build_commit_message(&sections).contains("Stack: React + Postgres"),
            "the line must survive a round trip through the commit message"
        );
    }
    /// The markers, unlike a label, let the section carry a rule and a
    /// sentence with punctuation the label grammar would reject.
    #[test]
    fn test_stack_section_may_contain_a_rule_and_prose() {
        let body = build_github_body(&stacked_sections());
        let parsed = parse_message(&body, MessageSection::Summary);

        assert_eq!(
            parsed.get(&MessageSection::Stack),
            Some(&STACK_TEXT.to_string())
        );
        assert!(STACK_TEXT.contains("---"));
        assert!(STACK_TEXT.contains("PR stack, review from the bottom up:"));
    }

    /// Bodies written before the markers labelled the section, and are still on
    /// GitHub, so the label is still honoured on read — and with it the label's
    /// hazard: a summary line shaped like it is still claimed by the section.
    /// The markers are what new bodies use; they are not a fix for that.
    #[test]
    fn test_a_plain_stack_label_is_still_honoured() {
        let sections = parse_message("PR Stack: this is my prose", MessageSection::Summary);

        assert_eq!(
            sections.get(&MessageSection::Stack),
            Some(&"this is my prose".to_string())
        );
    }

    /// A marker with prose around it is someone mentioning the marker, not a
    /// delimiter.
    #[test]
    fn test_a_marker_not_alone_on_its_line_is_prose() {
        let body =
            "the summary\n\nthe <!-- spr-stack --> marker opens it\n- #1\n<!-- /spr-stack -->";
        let parsed = parse_message(body, MessageSection::Summary);

        assert_eq!(parsed.get(&MessageSection::Stack), None);
        assert!(
            parsed
                .get(&MessageSection::Summary)
                .unwrap()
                .contains("the <!-- spr-stack --> marker opens it")
        );
    }

    /// Everything between the markers belongs to the section, including lines
    /// that would otherwise read as a label.
    #[test]
    fn test_markers_shield_their_contents_from_the_label_grammar() {
        let body =
            "the summary\n\n<!-- spr-stack -->\nSummary: not really\n- #1\n<!-- /spr-stack -->";
        let parsed = parse_message(body, MessageSection::Summary);

        assert_eq!(
            parsed.get(&MessageSection::Summary),
            Some(&"the summary".to_string())
        );
        assert_eq!(
            parsed.get(&MessageSection::Stack),
            Some(&"Summary: not really\n- #1".to_string())
        );
    }

    /// An unterminated marker must not swallow the rest of the body — doing so
    /// would take the `Pull Request:` line with it, and a change whose PR link
    /// has vanished gets a second PR opened for it.
    #[test]
    fn test_an_unterminated_marker_is_ordinary_text() {
        let body = "the summary\n\n<!-- spr-stack -->\n- #1\n\nPull Request: https://x/1";
        let parsed = parse_message(body, MessageSection::Summary);

        assert_eq!(parsed.get(&MessageSection::Stack), None);
        assert_eq!(
            parsed.get(&MessageSection::PullRequest),
            Some(&"https://x/1".to_string()),
            "the tail of the body must survive an unterminated marker"
        );
    }

    /// A commit message is never searched for a stack section, so one that
    /// quotes the markers — this feature's own documentation, say — keeps its
    /// text.
    #[test]
    fn test_a_commit_message_quoting_the_markers_is_untouched() {
        let msg = "feat: docs\n\nThe body looks like:\n\n```\n<!-- spr-stack -->\n- #1\n<!-- /spr-stack -->\n```\n\nPull Request: https://x/1";
        let parsed = parse_message(msg, MessageSection::Title);

        assert_eq!(parsed.get(&MessageSection::Stack), None);
        let summary = parsed.get(&MessageSection::Summary).unwrap();
        assert!(
            summary.contains("<!-- spr-stack -->"),
            "summary was: {summary}"
        );
        assert!(summary.contains("- #1"), "summary was: {summary}");
        assert_eq!(
            parsed.get(&MessageSection::PullRequest),
            Some(&"https://x/1".to_string())
        );
    }

    /// Only the section we wrote is taken. `build_message` renders it last, so
    /// an earlier marker pair is someone quoting the format.
    #[test]
    fn test_the_last_marker_pair_is_the_stack_section() {
        let body = "quoting the format:\n\n<!-- spr-stack -->\n- #999\n<!-- /spr-stack -->\n\n<!-- spr-stack -->\n- #1\n<!-- /spr-stack -->";
        let parsed = parse_message(body, MessageSection::Summary);

        assert_eq!(
            parsed.get(&MessageSection::Stack),
            Some(&"- #1".to_string())
        );
    }

    /// Prose written below the section on GitHub must survive. It moves above
    /// the section, which is rendered last, but it is not lost.
    #[test]
    fn test_prose_below_the_section_is_kept() {
        let body = format!(
            "the summary\n\n<!-- spr-stack -->\n{STACK_TEXT}\n<!-- /spr-stack -->\n\nPlease review carefully."
        );
        let parsed = parse_message(&body, MessageSection::Summary);

        assert_eq!(
            parsed.get(&MessageSection::Summary),
            Some(&"the summary\n\nPlease review carefully.".to_string())
        );
        assert_eq!(
            parsed.get(&MessageSection::Stack),
            Some(&STACK_TEXT.to_string())
        );
    }

    /// The body we render must parse back to the sections it came from, or a
    /// PR would look modified on every run.
    #[test]
    fn test_github_body_round_trips_through_parse() {
        let sections = stacked_sections();
        let body = build_github_body(&sections);

        let parsed = parse_message(&body, MessageSection::Summary);

        assert_eq!(
            parsed.get(&MessageSection::Summary),
            Some(&"the summary".to_string())
        );
        assert_eq!(
            parsed.get(&MessageSection::Stack),
            Some(&STACK_TEXT.to_string())
        );
        assert_eq!(build_github_body(&parsed), body);
    }

    #[test]
    fn test_parse_cherry_pick() {
        let map = parse_message(
            "My title\n\nPull Request: https://github.com/x/y/pull/1\nCherry Pick: true",
            MessageSection::Title,
        );
        assert_eq!(
            map.get(&MessageSection::Title).map(String::as_str),
            Some("My title")
        );
        assert_eq!(
            map.get(&MessageSection::PullRequest).map(String::as_str),
            Some("https://github.com/x/y/pull/1")
        );
        assert_eq!(
            map.get(&MessageSection::CherryPick).map(String::as_str),
            Some("true")
        );
    }

    #[test]
    fn test_build_commit_message_with_cherry_pick() {
        let map: MessageSectionsMap = [
            (MessageSection::Title, "My title".to_string()),
            (
                MessageSection::PullRequest,
                "https://github.com/x/y/pull/1".to_string(),
            ),
            (MessageSection::CherryPick, "true".to_string()),
        ]
        .into();
        let output = build_commit_message(&map);
        // Cherry Pick must appear after Pull Request
        let pr_pos = output.find("Pull Request:").unwrap();
        let cp_pos = output.find("Cherry Pick:").unwrap();
        assert!(
            cp_pos > pr_pos,
            "Cherry Pick should appear below Pull Request"
        );
        assert!(output.ends_with("Cherry Pick: true"));
    }

    #[test]
    fn test_roundtrip_cherry_pick() {
        let original = "My title\n\nPull Request: https://github.com/x/y/pull/1\nCherry Pick: true";
        let map = parse_message(original, MessageSection::Title);
        let rebuilt = build_commit_message(&map);
        let reparsed = parse_message(&rebuilt, MessageSection::Title);
        assert_eq!(
            reparsed
                .get(&MessageSection::CherryPick)
                .map(String::as_str),
            Some("true")
        );
        assert_eq!(
            reparsed
                .get(&MessageSection::PullRequest)
                .map(String::as_str),
            Some("https://github.com/x/y/pull/1")
        );
    }

    #[test]
    fn test_build_github_body_for_merging_omits_cherry_pick() {
        let map: MessageSectionsMap = [
            (MessageSection::Title, "My title".to_string()),
            (MessageSection::Summary, "Some summary".to_string()),
            (
                MessageSection::PullRequest,
                "https://github.com/x/y/pull/1".to_string(),
            ),
            (MessageSection::CherryPick, "true".to_string()),
        ]
        .into();
        let body = build_github_body_for_merging(&map);
        assert!(
            !body.contains("Cherry Pick"),
            "GitHub body should not contain Cherry Pick marker"
        );
    }

    #[test]
    fn test_build_message_trims() {
        assert_eq!(
            build_message(
                &[
                    (MessageSection::Title, "  Hello".to_string()),
                    (MessageSection::Summary, "Foo Bar  ".to_string())
                ]
                .into(),
                &[MessageSection::Title, MessageSection::Summary]
            ),
            "Hello\n\nFoo Bar"
        );
        assert_eq!(
            build_message(
                &[
                    (MessageSection::Title, "only title".to_string()),
                    (MessageSection::Summary, "\n".to_string())
                ]
                .into(),
                &[MessageSection::Summary]
            ),
            ""
        );
    }
}
