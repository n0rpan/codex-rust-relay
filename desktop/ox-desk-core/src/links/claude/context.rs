//! Model-visible OX context for a stock Claude Code process.
//!
//! Stable facts are appended to the system prompt once per process through the
//! stream-JSON `initialize` request. Per-submit facts (which OX interface sent a
//! message) travel through a `UserPromptSubmit` hook callback registered in the
//! same request, so OX never adds a settings flag to the launch.
//!
//! Stock Claude 2.1.331 and 2.1.332 were probed against a local mock provider:
//! - The callback input carries the prompt text but not the submitted `uuid`.
//!   The text is the message's text blocks joined by newlines and trimmed;
//!   images contribute nothing.
//! - Messages queued during a busy turn can be merged into one turn with one
//!   callback whose prompt joins their texts with a blank line.
//! - Local slash commands fire no callback. An interrupt keeps queued messages.
//! - An error reply lets the prompt proceed when `onFailure` is `continue`, but a
//!   timeout or a reply naming another hook event drops the user's message.
//!
//! So the link answers every callback immediately with a well-formed
//! `UserPromptSubmit` reply, and correlates it with accepted submits by
//! normalized text in submit order. When the text cannot identify the submit
//! unambiguously, the reply states that the interface is unknown, which never
//! attributes a message to the wrong interface or lets an older label carry over.
use super::Billing;
use serde_json::{Value, json};
use std::collections::VecDeque;

/// The callback ID OX registers for its `UserPromptSubmit` hook.
pub const CALLBACK_ID: &str = "ox_submit_context";
/// Seconds Claude waits for the answer. The reader answers on receipt, so this
/// only bounds a stalled host; stock Claude drops the prompt when it elapses.
const TIMEOUT_SECONDS: u64 = 30;
/// Submits Claude never reports (local slash commands) are dropped from the
/// front once a later submit matches; the bound covers a session that only
/// ever sends those.
const PENDING_LIMIT: usize = 64;
/// Caller-supplied context is a short descriptive label, never a document.
const CONTEXT_LIMIT: usize = 2048;
pub const UNKNOWN_SENDER: &str = "OX could not determine which interface sent this message.";

/// The stable facts appended to the system prompt for this process.
pub fn stable_facts(billing: &Billing, worker: bool) -> String {
    let route = match billing {
        Billing::Subscription => "Claude Code on the signed-in Claude subscription".to_owned(),
        Billing::Api(alias) => {
            format!("Claude Code through the OX API route `{alias}`, billed to its saved API key")
        }
    };
    let role = if worker {
        "You are an OX worker launched by another agent; your task arrives as the first message."
    } else {
        "People reach this conversation from OX Desktop or the OX phone interface."
    };
    format!(
        "# OX\nThis session is hosted by OX and uses {route}. {role} When OX knows which interface sent a message, it states that in context attached to that message; otherwise treat the interface as unknown and do not assume an earlier message's interface. For OX settings such as providers, accounts and remote access, run `ox help settings` (or `ox help` when that topic is unavailable). Use `ox doctor` for setup problems."
    )
}

/// The initialize request fields that deliver OX context.
pub fn initialize_fields(billing: &Billing, worker: bool) -> Value {
    json!({
        "appendSystemPrompt": stable_facts(billing, worker),
        "hooks": {"UserPromptSubmit": [{
            "hookCallbackIds": [CALLBACK_ID],
            "onFailure": "continue",
            "timeout": TIMEOUT_SECONDS,
        }]},
    })
}

/// The reply to a `UserPromptSubmit` callback.
pub fn reply(context: &str) -> Value {
    json!({"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":context}})
}

/// The text Claude reports as the prompt for one submitted message.
fn prompt_text(content: &[Value]) -> String {
    let text: Vec<&str> = content
        .iter()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect();
    normalize(&text.join("\n"))
}
/// Whitespace-insensitive comparison: Claude trims each message and joins a
/// merged batch with blank lines. Collisions only widen the ambiguity check.
fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}
fn label(context: Option<&str>) -> Option<String> {
    let context = context?.trim();
    if context.is_empty() {
        return None;
    }
    let mut end = context.len().min(CONTEXT_LIMIT);
    while !context.is_char_boundary(end) {
        end -= 1;
    }
    Some(context[..end].to_owned())
}

struct Pending {
    seq: u64,
    prompt: String,
    context: Option<String>,
}

/// Accepted submits awaiting their callback, in stdin order.
#[derive(Default)]
pub struct Ledger {
    pending: VecDeque<Pending>,
    seq: u64,
}

impl Ledger {
    /// Records a submit before it is written; returns a handle for `withdraw`.
    pub fn submit(&mut self, content: &[Value], context: Option<&str>) -> u64 {
        self.seq += 1;
        if self.pending.len() >= PENDING_LIMIT {
            self.pending.pop_front();
        }
        self.pending.push_back(Pending {
            seq: self.seq,
            prompt: prompt_text(content),
            context: label(context),
        });
        self.seq
    }
    /// Forgets a submit that could not be written.
    pub fn withdraw(&mut self, seq: u64) {
        self.pending.retain(|p| p.seq != seq);
    }
    /// The context for the turn Claude is about to start with `prompt`.
    pub fn answer(&mut self, prompt: &str) -> String {
        let prompt = normalize(prompt);
        // Every contiguous run of pending submits whose joined text is the prompt.
        let mut runs = Vec::new();
        for start in 0..self.pending.len() {
            let mut joined = String::new();
            for (end, pending) in self.pending.iter().enumerate().skip(start) {
                let text = &pending.prompt;
                if !text.is_empty() {
                    if !joined.is_empty() {
                        joined.push(' ');
                    }
                    joined.push_str(text);
                }
                if joined.len() > prompt.len() {
                    break;
                }
                if joined == prompt {
                    runs.push((start, end));
                }
            }
        }
        let Some(&(_, first_end)) = runs.first() else {
            // Not an OX submit (or already forgotten): attribute nothing.
            return UNKNOWN_SENDER.to_owned();
        };
        let rendered: Vec<String> = runs.iter().map(|&(s, e)| self.render(s, e)).collect();
        // Claude reports submits in order, so earlier unmatched ones never will be.
        self.pending.drain(..=first_end);
        if rendered.iter().all(|r| *r == rendered[0]) {
            rendered[0].clone()
        } else {
            UNKNOWN_SENDER.to_owned()
        }
    }
    fn render(&self, start: usize, end: usize) -> String {
        let labels: Vec<Option<&str>> = self
            .pending
            .range(start..=end)
            .map(|p| p.context.as_deref())
            .collect();
        if labels.iter().all(|l| *l == labels[0]) {
            return labels[0].unwrap_or(UNKNOWN_SENDER).to_owned();
        }
        let mut out = format!(
            "This turn combines {} messages, in the order they were sent.",
            labels.len()
        );
        for (i, l) in labels.iter().enumerate() {
            out.push_str(&format!(
                "\nMessage {}: {}",
                i + 1,
                l.unwrap_or(UNKNOWN_SENDER)
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESKTOP: &str = "This message was sent from OX Desktop.";
    const PHONE: &str = "This message was sent from the OX phone interface.";
    fn text(s: &str) -> Vec<Value> {
        vec![json!({"type":"text","text":s})]
    }

    #[test]
    fn a_queued_phone_message_keeps_its_label_while_the_desktop_is_active() {
        let mut l = Ledger::default();
        l.submit(&text("first"), Some(DESKTOP));
        l.submit(&text("from phone"), Some(PHONE));
        assert_eq!(l.answer("first"), DESKTOP);
        assert_eq!(l.answer("from phone"), PHONE);
        assert!(l.pending.is_empty());
    }

    #[test]
    fn merged_batches_are_labelled_per_message_or_once_when_equal() {
        let mut l = Ledger::default();
        l.submit(&text("second"), Some(PHONE));
        l.submit(&text("third"), Some(DESKTOP));
        let both = l.answer("second\n\nthird");
        assert!(both.contains("combines 2 messages"), "{both}");
        assert!(both.find(PHONE).unwrap() < both.find(DESKTOP).unwrap());
        l.submit(&text("a"), Some(PHONE));
        l.submit(&text("b"), Some(PHONE));
        assert_eq!(l.answer("a\n\nb"), PHONE);
    }

    #[test]
    fn ambiguous_or_unknown_prompts_never_borrow_a_label() {
        let mut l = Ledger::default();
        l.submit(&text("same"), Some(DESKTOP));
        l.submit(&text("same"), Some(PHONE));
        // Either submit could be meant: say so, and consume only the first.
        assert_eq!(l.answer("same"), UNKNOWN_SENDER);
        assert_eq!(l.answer("same"), PHONE);
        assert_eq!(l.answer("never sent"), UNKNOWN_SENDER);
        l.submit(&text("plain"), None);
        assert_eq!(l.answer("plain"), UNKNOWN_SENDER);
        l.submit(&text("labelled"), Some("  "));
        assert_eq!(l.answer("labelled"), UNKNOWN_SENDER);
    }

    #[test]
    fn unreported_slash_commands_and_withdrawn_writes_are_skipped() {
        let mut l = Ledger::default();
        l.submit(&text("/cost"), Some(DESKTOP));
        let failed = l.submit(&text("lost"), Some(DESKTOP));
        l.withdraw(failed);
        l.submit(&text("next"), Some(PHONE));
        assert_eq!(l.answer("next"), PHONE);
        assert!(l.pending.is_empty());
        for _ in 0..PENDING_LIMIT * 2 {
            l.submit(&text("/cost"), None);
        }
        assert_eq!(l.pending.len(), PENDING_LIMIT);
    }

    #[test]
    fn prompt_text_matches_the_stock_callback_shapes() {
        let mut l = Ledger::default();
        let png = json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":"AA=="}});
        l.submit(
            &[
                json!({"type":"text","text":"line a"}),
                json!({"type":"text","text":"line b"}),
            ],
            Some(DESKTOP),
        );
        assert_eq!(l.answer("line a\nline b"), DESKTOP);
        l.submit(
            &[png.clone(), json!({"type":"text","text":"look  "})],
            Some(PHONE),
        );
        assert_eq!(l.answer("look"), PHONE);
        l.submit(&[png], Some(DESKTOP));
        assert_eq!(l.answer(""), DESKTOP);
        l.submit(&text("  padded\n"), Some(PHONE));
        assert_eq!(l.answer("padded"), PHONE);
    }

    #[test]
    fn a_queued_image_only_message_makes_a_differently_labelled_prompt_ambiguous() {
        let png = json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":"AA=="}});
        let mut l = Ledger::default();
        l.submit(&text("words"), Some(DESKTOP));
        l.submit(std::slice::from_ref(&png), Some(PHONE));
        // Claude may have merged the image into this turn without changing its text.
        assert_eq!(l.answer("words"), UNKNOWN_SENDER);
        let mut l = Ledger::default();
        l.submit(&text("words"), Some(PHONE));
        l.submit(&[png], Some(PHONE));
        assert_eq!(l.answer("words"), PHONE);
    }

    #[test]
    fn caller_context_is_bounded() {
        let mut l = Ledger::default();
        let long = "é".repeat(CONTEXT_LIMIT);
        l.submit(&text("x"), Some(&long));
        let got = l.answer("x");
        assert!(got.len() <= CONTEXT_LIMIT && !got.is_empty());
    }

    #[test]
    fn stable_facts_name_the_route_without_credentials() {
        let sub = stable_facts(&Billing::Subscription, false);
        assert!(sub.contains("Claude subscription") && sub.contains("OX Desktop"));
        assert!(sub.contains("ox help settings"));
        let api = stable_facts(&Billing::Api("fixture/demo".into()), true);
        assert!(api.contains("`fixture/demo`") && api.contains("OX worker"));
        let init = initialize_fields(&Billing::Subscription, false);
        assert_eq!(
            init["hooks"]["UserPromptSubmit"][0]["onFailure"],
            "continue"
        );
        assert_eq!(
            reply("x")["hookSpecificOutput"]["hookEventName"],
            "UserPromptSubmit"
        );
    }
}
