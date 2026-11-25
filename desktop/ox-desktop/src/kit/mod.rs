//! Shared keyboard-accessible controls for the workspace and preferences.
use crate::appearance::Palette;
use gpui::{
    App, AppContext, ClickEvent, ElementId, InteractiveElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window, px, rgb,
};
use gpui_component::{
    Disableable,
    button::{Button, ButtonVariant, ButtonVariants},
};

pub fn button(
    palette: Palette,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    primary: bool,
    enabled: bool,
    click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    let variant = if primary {
        ButtonVariant::Primary
    } else {
        ButtonVariant::Secondary
    };
    let (id, label) = (id.into(), label.into());
    let button = Button::new(id.clone())
        .with_variant(variant)
        .label(label.clone())
        .h(px(32.))
        .border_1()
        .border_color(rgb(if primary {
            palette.accent
        } else {
            palette.line
        }))
        .disabled(!enabled)
        .tab_stop(enabled)
        .on_click(click);
    crate::platform::layout_probe::mark(button, &id, &label, enabled)
}

/// A quiet toolbar control retains Button's tab stop, accessible label, and activation behavior.
pub fn toolbar_button(
    p: Palette,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    enabled: bool,
    click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    let (id, label) = (id.into(), label.into());
    let button = Button::new(id.clone())
        .ghost()
        .label(label.clone())
        .h(px(30.))
        .rounded(px(3.))
        .text_color(rgb(p.muted))
        .disabled(!enabled)
        .tab_stop(enabled)
        .on_click(click);
    crate::platform::layout_probe::mark(button, &id, &label, enabled)
}

pub use gpui_component::menu::{DropdownMenu, PopupMenuItem};

pub fn row() -> gpui::Div {
    gpui::div().flex().flex_row().items_center()
}
pub fn col() -> gpui::Div {
    gpui::div().flex().flex_col()
}
pub fn small(p: Palette, label: impl Into<SharedString>) -> gpui::Div {
    gpui::div()
        .text_size(px(12.))
        .text_color(rgb(p.muted))
        .child(label.into())
}
pub fn copy_message(p: Palette, id: &'static str, message: &str) -> Button {
    let text = message.to_owned();
    toolbar_button(p, id, "Copy", true, move |_, _, cx| {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone()));
    })
    .accessibility_label("Copy message")
    .tooltip("Copy message")
}
pub fn icon_button(
    p: Palette,
    id: impl Into<ElementId>,
    glyph: &'static str,
    label: &'static str,
    click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    button(p, id, glyph, false, true, click)
        .w(px(32.))
        .tooltip(label)
}
pub fn sheet(p: Palette, id: &'static str) -> gpui::Stateful<gpui::Div> {
    col()
        .id(id)
        .overflow_y_scroll()
        .flex_shrink_0()
        .gap_2()
        .p_3()
        .border_l_1()
        .border_color(rgb(p.line))
        .bg(rgb(p.panel))
        .w(px(325.))
        .h_full()
}
pub fn field(
    initial: &str,
    hint: &str,
    secure: bool,
    window: &mut Window,
    cx: &mut App,
) -> gpui::Entity<gpui_component::input::InputState> {
    use gpui_component::input::InputState;
    cx.new(|cx| {
        InputState::new(window, cx)
            .default_value(initial.to_owned())
            .masked(secure)
            .placeholder(hint.to_owned())
    })
}

pub struct ListRow {
    pub cartridge: crate::activity::Cartridge,
    pub title: String,
    pub route: String,
    pub age: String,
    pub status: Option<&'static str>,
    pub selected: bool,
}
impl ListRow {
    fn secondary(&self) -> String {
        let mut label = self.route.clone();
        for part in self
            .status
            .into_iter()
            .chain(Some(self.age.as_str()))
            .filter(|s| !s.is_empty())
        {
            label.push_str(" · ");
            label.push_str(part);
        }
        label
    }
    pub fn render(
        self,
        p: Palette,
        id: impl Into<ElementId>,
        click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> gpui::Div {
        use gpui::prelude::FluentBuilder;
        let hint = format!("{}\n{}", self.title, self.secondary());
        let ink = if self.selected { p.accent } else { p.fg };
        let active = self.status.filter(|s| !matches!(*s, "Unread" | "Running"));
        let marker = match self.status {
            Some("Needs you") => p.warning,
            Some("Error") => p.error,
            _ => p.accent,
        };
        let track = row()
            .w_full()
            .h(px(37.))
            .gap_2()
            .px_2()
            .border_l_2()
            .border_color(rgb(if self.selected { p.accent } else { p.nav }))
            .when(self.selected, |d| {
                d.bg(rgb(crate::appearance::mix(p.nav, p.accent, 14)))
            })
            .child(crate::activity::cartridge(p, self.cartridge, self.selected))
            .child(
                gpui::div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(14.))
                    .font_weight(if self.selected {
                        gpui::FontWeight::SEMIBOLD
                    } else {
                        gpui::FontWeight::NORMAL
                    })
                    .text_color(rgb(ink))
                    .child(self.title),
            )
            .when_some(active, |d, status| {
                d.child(small(p, status).text_color(rgb(marker)).flex_shrink_0())
            })
            .when(self.status == Some("Running"), |d| {
                d.child(row().items_end().gap(px(2.)).children(
                    [4., 8., 12.].map(|h| gpui::div().w(px(3.)).h(px(h)).bg(rgb(marker))),
                ))
            })
            .when(self.status == Some("Unread"), |d| {
                d.child(gpui::div().w(px(5.)).h(px(5.)).bg(rgb(marker)))
            });
        col().h(px(39.)).pr_2().justify_center().child(
            Button::new(id)
                .ghost()
                .tab_stop(true)
                .w_full()
                .p_0()
                .h(px(37.))
                .child(track)
                .tooltip(hint)
                .on_click(click),
        )
    }
}

#[cfg(test)]
mod test {
    use super::*;
    // Snapshot the same two labels and selection state passed to the native list control.
    fn text_tree(row: &ListRow) -> String {
        format!(
            "list row\n  button{} {}\n  text {}",
            if row.selected { " [selected]" } else { "" },
            row.title,
            row.secondary()
        )
    }
    #[test]
    fn selected_thread_and_background_approval_have_distinct_labels() {
        let mut row = ListRow {
            cartridge: crate::activity::Cartridge::Codex,
            title: "Refactor".into(),
            route: "ChatGPT subscription".into(),
            age: "3m".into(),
            status: None,
            selected: true,
        };
        assert_eq!(
            text_tree(&row),
            "list row\n  button [selected] Refactor\n  text ChatGPT subscription · 3m"
        );
        row.selected = false;
        row.status = Some("Needs you");
        row.route = "OX API · fixture/demo".into();
        assert_eq!(
            text_tree(&row),
            "list row\n  button Refactor\n  text OX API · fixture/demo · Needs you · 3m"
        );
    }
}
