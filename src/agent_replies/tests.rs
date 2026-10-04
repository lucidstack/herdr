use std::path::PathBuf;

use super::*;
use crate::transcript::{last_message, TranscriptFormat};

fn key(text: &str) -> MessageKey {
    MessageKey::of(text).expect("the text has a letter or a digit")
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| item.to_string()).collect()
}

#[test]
fn a_message_keeps_its_key_whatever_sits_between_its_letters_and_digits() {
    let plain = "Merge the branch, then run the tests. Should I proceed?";
    let rendered = "**Merge** the branch,\n\n  then run `the tests`.\n- Should I proceed?\n";
    let curly = "Merge the branch — then run the “tests”… Should I proceed ?";

    assert_eq!(key(plain), key(rendered));
    assert_eq!(key(plain), key(curly));
}

#[test]
fn messages_with_other_letters_or_another_order_have_other_keys() {
    assert_ne!(key("Merge it now"), key("Merge it"));
    assert_ne!(key("Merge it"), key("Merge them"));
    // The order counts: a key that sorted the characters would take these for one message.
    assert_ne!(key("listen"), key("silent"));
}

#[test]
fn unicode_letters_and_digits_count_in_a_key() {
    assert_ne!(key("café"), key("cafe"));
    assert_ne!(key("灯り"), key("灯"));
    assert_ne!(key("φῶς"), key("φως"));
    assert_ne!(key("Release 2026"), key("Release 2027"));
}

#[test]
fn letters_and_digits_inside_markup_count_in_a_key() {
    // A harness that rendered or flattened the markdown would drop them, and change the key.
    assert_ne!(
        key("Run it:\n```sh\nmake test\n```"),
        key("Run it: make test")
    );
    assert_ne!(
        key("See [the guide](https://example.com/guide)."),
        key("See the guide.")
    );
    assert_ne!(key("Press <kbd>Enter</kbd>."), key("Press Enter."));
}

#[test]
fn a_text_without_letters_or_digits_has_no_key() {
    for text in ["", "  \n\t", "— … !!!", "✨🌙"] {
        assert_eq!(MessageKey::of(text), None, "{text:?}");
    }
}

#[test]
fn a_long_message_is_hashed_as_the_plain_sequence_of_its_letters_and_digits() {
    // Longer than the block the digest is fed in, with multi-byte characters across its edges.
    let message = "né 灯り φῶς, ".repeat(400);
    let alphanumerics: String = message.chars().filter(|ch| ch.is_alphanumeric()).collect();

    assert_eq!(
        key(&message),
        MessageKey(format!("{:x}", Sha256::digest(alphanumerics.as_bytes())))
    );
}

#[test]
fn replies_are_trimmed() {
    let replies = normalize_replies(strings(&["  Yes, merge it \n", "\tNo"])).unwrap();

    assert_eq!(replies, strings(&["Yes, merge it", "No"]));
}

#[test]
fn no_replies_is_a_valid_report() {
    assert_eq!(normalize_replies(Vec::new()), Ok(Vec::new()));
}

#[test]
fn three_replies_are_accepted_and_a_fourth_is_refused() {
    assert!(normalize_replies(strings(&["a", "b", "c"])).is_ok());

    let error = normalize_replies(strings(&["a", "b", "c", "d"])).unwrap_err();

    assert_eq!(error, "replies may hold at most 3 items, got 4");
}

#[test]
fn a_reply_may_be_two_hundred_characters_not_bytes_and_no_more() {
    // 400 bytes, 200 characters.
    let longest = "é".repeat(200);
    assert_eq!(
        normalize_replies(vec![longest.clone()]),
        Ok(vec![longest.clone()])
    );
    // The limit applies to what is left once trimmed.
    assert_eq!(
        normalize_replies(vec![format!("  {longest}\n")]),
        Ok(vec![longest.clone()])
    );

    let error = normalize_replies(vec![format!("{longest}é")]).unwrap_err();

    assert_eq!(
        error,
        "replies[0] is 201 characters long; at most 200 are allowed"
    );
}

#[test]
fn an_empty_or_blank_reply_is_refused_and_named_by_its_position() {
    for blank in ["", " \n\t "] {
        let error = normalize_replies(strings(&["Yes", blank])).unwrap_err();

        assert_eq!(error, "replies[1] must not be empty");
    }
}

#[test]
fn replies_are_offered_only_for_the_message_they_were_reported_for() {
    let mut agent_replies = AgentReplies::default();
    agent_replies.report(
        key("Should I merge it?"),
        strings(&["Yes, merge it", "Show me the diff"]),
    );
    let offered = agent_replies.offered().unwrap();

    // The harness and the transcript write the same message differently.
    assert_eq!(
        offered.replies_for("**Should** I merge it?\n"),
        strings(&["Yes, merge it", "Show me the diff"])
    );
    // The transcript is behind, or the agent has moved on: another message.
    assert!(offered.replies_for("Should I rebase it?").is_empty());
}

#[test]
fn every_accepted_report_counts_and_the_newest_replaces_the_previous() {
    let mut agent_replies = AgentReplies::default();
    assert_eq!(agent_replies.revision(), 0);

    agent_replies.report(key("First question?"), strings(&["One"]));
    assert_eq!(agent_replies.revision(), 1);

    // An empty report says the agent offers nothing for this message; it still counts.
    agent_replies.report(key("Second question?"), Vec::new());
    assert_eq!(agent_replies.revision(), 2);
    let offered = agent_replies.offered().unwrap();
    assert_eq!(offered.message, key("Second question?"));
    assert!(offered.replies.is_empty());
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/agent-replies")
        .join(name)
}

/// The final message of a real turn as its harness reported it, and as Herdr reads it back from
/// the transcript the harness wrote.
fn harness_and_transcript_keys(turn: &str, format: TranscriptFormat) -> (MessageKey, MessageKey) {
    let harness = std::fs::read_to_string(fixture(&format!("{turn}.message.txt"))).unwrap();
    let read = last_message(format, &fixture(&format!("{turn}.jsonl")))
        .unwrap()
        .expect("the transcript ends in a finished message");
    (key(&harness), key(&read.text))
}

// The fixtures are turns recorded from Claude Code 2.1.289 and omp 18.5.0 on a fictional prompt.
// `.message.txt` is the Stop hook's `last_assistant_message` (Claude) or the text of the last
// assistant message of `agent_end` (omp); `.jsonl` is the transcript the harness wrote, reduced to
// the entries and fields the readers use. The final messages are markdown with a code block, and
// the plain turns add a table and non-Latin letters. A tool turn has an earlier response that
// stopped for a tool call.

#[test]
fn a_claude_tool_turns_stop_hook_message_has_the_key_of_the_message_herdr_reads() {
    let (harness, transcript) =
        harness_and_transcript_keys("claude-tool-turn", TranscriptFormat::Claude);

    assert_eq!(harness, transcript);
}

#[test]
fn a_claude_plain_turns_stop_hook_message_has_the_key_of_the_message_herdr_reads() {
    let (harness, transcript) =
        harness_and_transcript_keys("claude-plain-turn", TranscriptFormat::Claude);

    assert_eq!(harness, transcript);
}

#[test]
fn an_omp_tool_turns_final_text_has_the_key_of_the_message_herdr_reads() {
    let (harness, transcript) = harness_and_transcript_keys("omp-tool-turn", TranscriptFormat::Omp);

    assert_eq!(harness, transcript);
}

#[test]
fn an_omp_plain_turns_final_text_has_the_key_of_the_message_herdr_reads() {
    let (harness, transcript) =
        harness_and_transcript_keys("omp-plain-turn", TranscriptFormat::Omp);

    assert_eq!(harness, transcript);
}
