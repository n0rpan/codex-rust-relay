//! Presentation of stock transcript items. Collapsed rows and expanded previews do bounded work.
use crate::appearance::Palette;
use crate::appearance::Step;
use gpui::{prelude::FluentBuilder, *};
use gpui_component::{
    ActiveTheme, WindowExt,
    button::{Button, ButtonVariants},
    text::{TextView, TextViewStyle},
};
use ox_desk_core::transcript::{Entry, item::Item};
use std::fmt::{self, Write};

pub(super) const PAGE: usize = 12;
const PREVIEW_BYTES: usize = 6 * 1024;
const PREVIEW_LINES: usize = 40;

pub(super) fn markdown(view: TextView, project: Option<std::path::PathBuf>) -> TextView {
    let mut card = StyleRefinement::default()
        .pt(px(42.))
        .pb(px(14.))
        .px_4()
        .rounded_lg()
        .min_w_0()
        .max_h(px(360.));
    card.overflow.y = Some(Overflow::Scroll);
    card.overflow.x = Some(Overflow::Scroll);
    view.style(TextViewStyle::default().code_block(card))
        .code_block_actions(move |block, _, cx| {
            let code = block.code();
            let shell = match block.lang().as_deref() {
                Some("sh" | "shell") => Some("/bin/sh"),
                Some("bash") => Some("/bin/bash"),
                Some("zsh") => Some("/bin/zsh"),
                _ => None,
            };
            let review = project.clone().zip(shell);
            let script = code.clone();
            let p = Palette::get(cx);
            let mono = cx.theme().mono_font_family.clone();
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .font_family(mono.clone())
                        .text_size(p.text(Step::Meta))
                        .text_color(rgb(p.muted))
                        .child(block.lang().unwrap_or_else(|| "Code".into())),
                )
                .child(
                    Button::new("copy-code")
                        .ghost()
                        .compact()
                        .h(px(24.))
                        .label("Copy")
                        .accessibility_label("Copy code block")
                        .tooltip("Copy code block")
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(code.to_string()))
                        }),
                )
                .when_some(review, |bar,(dir,shell)| {
                    bar.child(Button::new("review-shell-block").ghost().compact().h(px(24.))
                        .label("Review in terminal")
                        .tooltip("Review this shell snippet before running it in the session directory")
                        .on_click(move |_, window, cx| {
                            let command=crate::terminal::TerminalCommand {
                                program:shell.into(), args:vec!["-c".into(),script.as_ref().into()],
                            };
                            if let Err(message)=crate::terminal::open(dir.clone(),Some(command),|_,_|{},cx) {
                                window.push_notification(message,cx);
                            }
                        }))
                })
        })
}

fn kind(item: &Item) -> &'static str {
    match item {
        Item::CommandExecution { .. } => "Terminal",
        Item::FileChange { .. } => "Files",
        Item::AgentOperation { .. } => "Agent",
        Item::McpToolCall { .. } => "MCP",
        Item::WebSearch { .. } => "Search",
        _ => "Tool",
    }
}

fn status(item: &Item) -> &str {
    let status = match item {
        Item::CommandExecution { status, .. }
        | Item::FileChange { status, .. }
        | Item::AgentOperation { status, .. }
        | Item::McpToolCall { status, .. }
        | Item::ToolCall { status, .. }
        | Item::Unknown { status, .. } => status.as_str(),
        _ => "",
    };
    match status {
        "inProgress" | "running" => "Running",
        "completed" | "success" => "Done",
        "failed" | "error" => "Failed",
        "cancelled" | "canceled" => "Canceled",
        "declined" => "Declined",
        other => other,
    }
}

fn compact(value: &str) -> String {
    let mut result: String = value
        .chars()
        .take(96)
        .take_while(|c| !matches!(c, '\r' | '\n'))
        .collect();
    if result.len() < value.len() {
        result.push('…');
    }
    result
}

fn shell_summary(mut command: &str) -> String {
    // Stock Codex records the invocation and often wraps its script in a login shell.
    // Limit this shortcut to recognizable shells; details always keep the original.
    if let Some(split) = command[..command.floor_char_boundary(128.min(command.len()))].find(' ')
        && let (shell, args) = (&command[..split], &command[split + 1..])
        && matches!(
            shell.rsplit('/').next(),
            Some("sh" | "bash" | "zsh" | "fish")
        )
        && let Some(script) = args
            .strip_prefix("-lc ")
            .or_else(|| args.strip_prefix("-c "))
    {
        command = script
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .or_else(|| script.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
            .unwrap_or(script);
    }
    compact(command)
}

fn summary(item: &Item) -> String {
    match item {
        Item::CommandExecution { command, .. } => shell_summary(command),
        Item::FileChange { changes, .. } => match changes.as_slice() {
            [one] => compact(&one.path),
            multiple => format!("{} changed files", multiple.len()),
        },
        Item::AgentOperation { tool, .. } | Item::ToolCall { tool, .. } => compact(tool),
        Item::McpToolCall { server, tool, .. } => {
            format!("{} / {}", compact(server), compact(tool))
        }
        Item::WebSearch { query } => compact(query),
        Item::Unknown { kind, .. } => compact(kind),
        _ => String::new(),
    }
}

pub(super) fn header(
    index: usize,
    latest: &Entry,
    count: usize,
    expanded: bool,
    p: Palette,
    mono: &SharedString,
    toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    let failed_exit = match latest.item {
        Item::CommandExecution {
            exit_code: Some(n), ..
        } if n != 0 => Some(n),
        _ => None,
    };
    let status = status(&latest.item);
    let status_label = failed_exit.map_or_else(|| compact(status), |n| format!("Exit {n}"));
    let (title, detail) = if count == 1 {
        (kind(&latest.item).to_owned(), summary(&latest.item))
    } else {
        (
            format!("{count} tool calls"),
            format!(
                "Latest · {} · {}",
                kind(&latest.item),
                summary(&latest.item)
            ),
        )
    };
    let color = match status {
        _ if failed_exit.is_some() => p.error,
        "Failed" | "Declined" => p.error,
        "Running" => p.accent,
        _ => p.muted,
    };
    Button::new(("tool-group", index))
        .ghost()
        .compact()
        .w_full()
        .min_w_0()
        .h(px(31.))
        .px_2()
        .overflow_hidden()
        .accessibility_label(format!("{title} {detail} {status}"))
        .on_click(toggle)
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .w_full()
                .min_w_0()
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(rgb(p.muted))
                        .child(if expanded { "▾" } else { "▸" }),
                )
                .child(
                    div()
                        .text_size(p.text(Step::Ui))
                        .flex_shrink_0()
                        .child(title),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family(mono.clone())
                        .text_size(p.text(Step::Meta))
                        .text_color(rgb(p.muted))
                        .child(detail),
                )
                .when(!status_label.is_empty(), |d| {
                    d.child(
                        div()
                            .flex_shrink_0()
                            .text_size(p.text(Step::Meta))
                            .text_color(rgb(color))
                            .child(status_label),
                    )
                }),
        )
}

fn write_detail(item: &Item, output: &str, to: &mut impl Write) -> fmt::Result {
    match item {
        Item::CommandExecution {
            command,
            aggregated_output,
            exit_code,
            ..
        } => {
            write!(to, "$ {command}")?;
            if let Some(text) = aggregated_output.as_deref().filter(|text| !text.is_empty()) {
                write!(to, "\n{text}")?;
            }
            if let Some(code) = exit_code {
                write!(to, "\nExit {code}")?;
            }
        }
        Item::FileChange { changes, .. } => {
            for (i, c) in changes.iter().enumerate() {
                if i > 0 {
                    to.write_char('\n')?;
                }
                write!(to, "{}\n{}", c.path, c.diff)?;
            }
        }
        Item::AgentOperation {
            tool,
            receiver_thread_ids,
            ..
        } => {
            to.write_str(tool)?;
            for id in receiver_thread_ids {
                write!(to, "\n{id}")?;
            }
        }
        Item::McpToolCall { server, tool, .. } => write!(to, "{server} / {tool}")?,
        Item::WebSearch { query } => to.write_str(query)?,
        Item::ToolCall { tool, .. } => to.write_str(tool)?,
        Item::Unknown { kind, status } => write!(to, "{kind} {status}")?,
        _ => (),
    }
    if !output.is_empty() {
        write!(to, "\n{output}")?;
    }
    Ok(())
}

pub(super) fn full_text(entry: &Entry) -> String {
    let mut result = String::new();
    write_detail(&entry.item, &entry.output, &mut result).expect("writing to a string");
    result
}
#[derive(Default)]
struct Preview {
    text: String,
    newlines: usize,
}
impl Write for Preview {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let mut end = s.floor_char_boundary((PREVIEW_BYTES - self.text.len()).min(s.len()));
        for (i, _) in s[..end].match_indices('\n') {
            if self.newlines + 1 == PREVIEW_LINES {
                end = i;
                break;
            }
            self.newlines += 1;
        }
        self.text.push_str(&s[..end]);
        if end < s.len() {
            Err(fmt::Error)
        } else {
            Ok(())
        }
    }
}

pub(super) fn detail(
    entry: &Entry,
    index: usize,
    grouped: bool,
    p: Palette,
    mono: &SharedString,
    copy: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Div {
    let mut preview = Preview::default();
    let truncated = write_detail(&entry.item, &entry.output, &mut preview).is_err();
    let title = if grouped {
        format!(
            "{} · {} · {}",
            kind(&entry.item),
            summary(&entry.item),
            status(&entry.item)
        )
        .trim_end_matches(" · ")
        .to_owned()
    } else {
        "Details".to_owned()
    };
    div()
        .w_full()
        .min_w_0()
        .border_t_1()
        .border_color(rgb(p.line))
        .p_2()
        .child(
            div()
                .flex()
                .w_full()
                .min_w_0()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(p.text(Step::Meta))
                        .text_color(rgb(p.muted))
                        .child(title),
                )
                .when_some(
                    match entry.item {
                        Item::CommandExecution { exit_code, .. } => exit_code,
                        _ => None,
                    },
                    |d, code| {
                        d.child(
                            div()
                                .text_size(p.text(Step::Meta))
                                .flex_shrink_0()
                                .text_color(rgb(if code == 0 { p.muted } else { p.error }))
                                .child(format!("Exit {code}")),
                        )
                    },
                )
                .child(
                    Button::new(("copy-tool", index))
                        .ghost()
                        .compact()
                        .h(px(24.))
                        .label("Copy")
                        .accessibility_label("Copy full tool details")
                        .tooltip("Copy full tool details")
                        .on_click(copy),
                ),
        )
        .child(
            div()
                .id(("tool-output", index))
                .w_full()
                .min_w_0()
                .max_h(px(190.))
                .overflow_scroll()
                .bg(rgb(p.nav))
                .rounded_md()
                .p_2()
                .font_family(mono.clone())
                .text_size(p.text(Step::Ui))
                .whitespace_nowrap()
                .child(preview.text),
        )
        .when(truncated, |d| {
            d.child(
                div()
                    .text_size(p.text(Step::Meta))
                    .pt_1()
                    .text_color(rgb(p.muted))
                    .child("Preview truncated. Copy for the full output."),
            )
        })
}
pub(super) fn more(
    remaining: usize,
    click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    Button::new("more-tools")
        .ghost()
        .compact()
        .label(format!("Show {} more", remaining.min(PAGE)))
        .on_click(click)
}

#[cfg(test)]
mod tests {
    use super::{
        PREVIEW_BYTES, PREVIEW_LINES, Preview, compact, shell_summary, status, write_detail,
    };
    use ox_desk_core::transcript::item::Item;
    use std::fmt::Write;

    #[test]
    fn transcript_cards_summarize_stock_shells_without_changing_the_copy() {
        let command = "/usr/bin/bash -lc \"git status --short && echo '✓'\"";
        assert_eq!(shell_summary(command), "git status --short && echo '✓'");
        assert_eq!(
            shell_summary("python -c 'print(1)'"),
            "python -c 'print(1)'"
        );
        let item = Item::CommandExecution {
            cwd: None,
            command: command.into(),
            status: "completed".into(),
            aggregated_output: Some("answer".into()),
            exit_code: Some(0),
        };
        let mut copy = String::new();
        write_detail(&item, "", &mut copy).unwrap();
        assert_eq!(copy, format!("$ {command}\nanswer\nExit 0"));
        assert_eq!(status(&item), "Done");
        assert!(compact(&"界".repeat(130)).chars().count() <= 97);
    }

    #[test]
    fn transcript_cards_limit_only_the_preview() {
        let item = Item::ToolCall {
            tool: "fixture".into(),
            status: "failed".into(),
        };
        let output = format!("{}\nfull trailing line", "many lines\n".repeat(2000));
        let mut preview = Preview::default();
        assert!(write_detail(&item, &output, &mut preview).is_err());
        assert_eq!(preview.text.lines().count(), PREVIEW_LINES);
        let mut copy = String::new();
        write_detail(&item, &output, &mut copy).unwrap();
        assert_eq!(copy, format!("fixture\n{output}"));
        let mut wide = Preview::default();
        assert!(wide.write_str(&"🦀".repeat(PREVIEW_BYTES)).is_err());
        assert!(wide.text.len() <= PREVIEW_BYTES);
        assert!(wide.text.ends_with('🦀'));
    }

    #[test]
    fn transcript_cards_have_registered_native_grammars() {
        use gpui_component::highlighter::LanguageRegistry;
        let registry = LanguageRegistry::singleton();
        for language in ["bash", "sh", "rust", "python", "json"] {
            assert!(registry.language(language).is_some(), "{language}");
        }
        assert!(registry.language("unknown-fixture").is_none());
    }
}
