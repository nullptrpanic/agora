use super::super::lark_api::LARK_CARD_MAX_BYTES;
use super::{LarkCardContent, LarkProcessPhase, LarkProgressKind};
use crate::channel::bounded_tail;
use crate::i18n;
use serde_json::{Value, json};

// Budget the actual serialized representation, including UTF-8 and JSON escapes.
// Operate on the rendered snapshot, never clone or mutate the retained run history.
pub(super) fn fit(
    card: &mut Value,
    process_index: Option<usize>,
    latest_phase: Option<&LarkProcessPhase>,
) {
    if card.to_string().len() <= LARK_CARD_MAX_BYTES {
        return;
    }
    if let Some(elements) = card
        .pointer_mut("/body/elements")
        .and_then(Value::as_array_mut)
    {
        elements.push(json!({"tag": "markdown", "content": i18n::OUTPUT_TRUNCATED.trim()}));
    }
    let has_thinking = latest_phase.is_some_and(|phase| phase.thinking.is_some());
    let mut latest_progress = latest_phase
        .and_then(|phase| {
            phase
                .progress
                .iter()
                .find(|entry| entry.kind == LarkProgressKind::Message)
        })
        .map(|entry| {
            format!(
                "{}  {}",
                LarkCardContent::progress_marker(entry.status),
                entry.text
            )
        });
    let has_progress = latest_progress.is_some();
    while card.to_string().len() > LARK_CARD_MAX_BYTES {
        if let Some(elements) = process_index
            .and_then(|index| card.pointer_mut("/body/elements")?.get_mut(index))
            .and_then(|panel| panel.get_mut("elements"))
            .and_then(Value::as_array_mut)
            && trim_old_process(elements, &mut latest_progress, has_thinking, has_progress)
        {
            continue;
        }
        if let Some(panel) =
            process_index.and_then(|index| card.pointer_mut("/body/elements")?.get_mut(index))
            && let Some(text) = longest_display_text(panel).filter(|text| text.len() > 1024)
        {
            shrink_text(text);
            continue;
        }
        let Some(text) = longest_display_text(card).filter(|text| text.len() > 1024) else {
            break;
        };
        shrink_text(text);
    }
}

fn shrink_text(text: &mut String) {
    let budget = text.len() / 2;
    // Keep the section title / opening code fence, plus the newest UTF-8-safe tail.
    *text = if let Some((heading, body)) = text
        .split_once('\n')
        .filter(|(heading, _)| heading.len() < 512)
    {
        format!(
            "{heading}\n{}",
            bounded_tail(body.to_string(), budget, i18n::OUTPUT_TRUNCATED).0
        )
    } else {
        bounded_tail(std::mem::take(text), budget, i18n::OUTPUT_TRUNCATED).0
    };
}

fn trim_old_process(
    elements: &mut Vec<Value>,
    latest_progress: &mut Option<String>,
    has_thinking: bool,
    has_progress: bool,
) -> bool {
    // Phases are chronological, separated by hr. Within the newest phase,
    // commands are newest-first. Keep its summary and latest command if present.
    if let Some(separator) = elements.iter().position(|element| element["tag"] == "hr") {
        elements.drain(..=separator);
        return true;
    }
    // Grouped progress is newest-first, unlike the characters within one entry.
    // Use the semantic source entry instead of trying to parse its Markdown.
    if let Some(text) = latest_progress.take() {
        if elements.len() > usize::from(has_thinking)
            && elements
                .last()
                .is_some_and(|element| element["tag"] == "markdown")
        {
            elements.last_mut().unwrap()["content"] = Value::String(text);
        } else {
            elements.push(json!({"tag": "markdown", "content": text}));
        }
        return true;
    }
    let keep = usize::from(has_thinking) + usize::from(has_progress) + 1;
    if elements.len() > keep {
        elements.remove(elements.len() - 1 - usize::from(has_progress));
        true
    } else {
        false
    }
}

fn longest_display_text(value: &mut Value) -> Option<&mut String> {
    match value {
        Value::Object(fields) => fields
            .iter_mut()
            .filter_map(|(key, child)| {
                if key == "behaviors" {
                    None // Never rewrite opaque action IDs or command arguments.
                } else if key == "content" {
                    match child {
                        Value::String(text) => Some(text),
                        _ => None,
                    }
                } else {
                    longest_display_text(child)
                }
            })
            .max_by_key(|text| text.len()),
        Value::Array(children) => children
            .iter_mut()
            .filter_map(longest_display_text)
            .max_by_key(|text| text.len()),
        _ => None,
    }
}
