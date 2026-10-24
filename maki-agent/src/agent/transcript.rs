//! A session in the order a reader first saw it. That order differs from
//! history in one place: the line of an accepted observation.

use maki_providers::{Message, Role};

/// One step of a transcript, as frontends show it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TranscriptItem<'a> {
    Message(&'a Message),
    /// The line of the observation at `index` in history, such as a
    /// background result.
    ObservationLine {
        index: usize,
        text: &'a str,
    },
}

/// The line a reader sees for an accepted observation. The model reads only
/// the body, so an observation without a line stays hidden.
pub fn observation_line(message: &Message) -> Option<&str> {
    message
        .display_text
        .as_deref()
        .filter(|text| message.is_observation() && !text.is_empty())
}

/// History keeps an observation ahead of the prompts it landed with, but
/// frontends drew those prompts first, so its line waits for them. A reply
/// or a cancelled run's hidden marker closes those prompts. A run that fails
/// before any reply leaves neither, so its line moves past the next prompt.
pub fn display_order(messages: &[Message]) -> Vec<TranscriptItem<'_>> {
    let mut items = Vec::with_capacity(messages.len());
    let mut lines = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        if matches!(message.role, Role::Assistant) || message.display_text.as_deref() == Some("") {
            items.append(&mut lines);
        }
        if let Some(text) = observation_line(message) {
            lines.push(TranscriptItem::ObservationLine { index, text });
        }
        items.push(TranscriptItem::Message(message));
    }
    items.append(&mut lines);
    items
}

#[cfg(test)]
mod tests {
    use maki_providers::ContentBlock;
    use test_case::test_case;

    use super::*;

    const BODY: &str = "Background task find auth finished:\n\nok";
    const LINE: &str = "Background agent finished: find auth";
    const SECOND_LINE: &str = "Background agent failed: lint";
    const PROMPT: &str = "what changed";
    const NEXT_PROMPT: &str = "and then";
    const REPLY: &str = "the build is green";
    const CANCELLED: &str = "[Cancelled by user]";

    /// What a test history holds.
    #[derive(Clone, Copy)]
    enum Step {
        Notice(&'static str),
        Hidden,
        Prompt(&'static str),
        Reply,
        Cancelled,
    }

    fn message(step: Step) -> Message {
        match step {
            Step::Notice(line) => {
                let mut message = Message::observation(BODY.into());
                message.display_text = Some(line.into());
                message
            }
            Step::Hidden => Message::observation(BODY.into()),
            Step::Prompt(text) => Message::user(text.into()),
            Step::Reply => Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text { text: REPLY.into() }],
                ..Default::default()
            },
            Step::Cancelled => Message::synthetic(CANCELLED.into()),
        }
    }

    #[derive(Debug, PartialEq)]
    enum Row {
        Notice,
        Prompt,
        Reply,
        Hidden,
    }

    fn row(step: Step) -> (Row, &'static str) {
        match step {
            Step::Notice(line) => (Row::Notice, line),
            Step::Prompt(text) => (Row::Prompt, text),
            Step::Reply => (Row::Reply, REPLY),
            Step::Cancelled => (Row::Hidden, CANCELLED),
            Step::Hidden => unreachable!("an unlabelled observation shows nothing"),
        }
    }

    fn shown(history: &[Message]) -> Vec<(Row, &str)> {
        display_order(history)
            .into_iter()
            .filter_map(|item| match item {
                TranscriptItem::ObservationLine { index, text } => {
                    assert!(history[index].is_observation());
                    Some((Row::Notice, text))
                }
                TranscriptItem::Message(message) if message.is_observation() => None,
                TranscriptItem::Message(message) => {
                    let kind = match message.role {
                        Role::Assistant => Row::Reply,
                        _ if message.display_text.as_deref() == Some("") => Row::Hidden,
                        _ => Row::Prompt,
                    };
                    Some((kind, message.first_text_content().unwrap()))
                }
            })
            .collect()
    }

    #[test_case(&[Step::Notice(LINE), Step::Prompt(PROMPT), Step::Reply],
        &[Step::Prompt(PROMPT), Step::Notice(LINE), Step::Reply] ; "after_its_prompt")]
    #[test_case(&[Step::Notice(LINE), Step::Prompt(PROMPT), Step::Prompt(NEXT_PROMPT), Step::Reply],
        &[Step::Prompt(PROMPT), Step::Prompt(NEXT_PROMPT), Step::Notice(LINE), Step::Reply] ; "after_a_burst")]
    #[test_case(&[Step::Prompt(PROMPT), Step::Notice(LINE), Step::Notice(SECOND_LINE), Step::Prompt(NEXT_PROMPT), Step::Reply],
        &[Step::Prompt(PROMPT), Step::Prompt(NEXT_PROMPT), Step::Notice(LINE), Step::Notice(SECOND_LINE), Step::Reply] ; "two_notices_keep_their_order")]
    #[test_case(&[Step::Prompt(PROMPT), Step::Reply, Step::Notice(LINE), Step::Reply],
        &[Step::Prompt(PROMPT), Step::Reply, Step::Notice(LINE), Step::Reply] ; "wake_without_prompt")]
    #[test_case(&[Step::Notice(LINE), Step::Prompt(PROMPT), Step::Cancelled, Step::Prompt(NEXT_PROMPT)],
        &[Step::Prompt(PROMPT), Step::Notice(LINE), Step::Cancelled, Step::Prompt(NEXT_PROMPT)] ; "a_cancelled_run_keeps_its_line")]
    #[test_case(&[Step::Notice(LINE), Step::Prompt(PROMPT)],
        &[Step::Prompt(PROMPT), Step::Notice(LINE)] ; "trailing_line")]
    #[test_case(&[Step::Hidden, Step::Prompt(PROMPT), Step::Reply],
        &[Step::Prompt(PROMPT), Step::Reply] ; "unlabelled_stays_hidden")]
    fn lines_follow_the_prompts_they_landed_with(history: &[Step], expected: &[Step]) {
        let history: Vec<Message> = history.iter().copied().map(message).collect();

        let expected: Vec<_> = expected.iter().copied().map(row).collect();
        assert_eq!(shown(&history), expected);
    }

    #[test]
    fn every_message_keeps_its_place() {
        let history: Vec<Message> = [Step::Notice(LINE), Step::Prompt(PROMPT), Step::Reply]
            .into_iter()
            .map(message)
            .collect();

        let messages: Vec<&Message> = display_order(&history)
            .into_iter()
            .filter_map(|item| match item {
                TranscriptItem::Message(message) => Some(message),
                TranscriptItem::ObservationLine { .. } => None,
            })
            .collect();

        assert_eq!(messages, history.iter().collect::<Vec<_>>());
    }
}
