//! Shared keyboard-accessible controls for the workspace and preferences.
use crate::appearance::{LABEL_FONT, Palette, Step};
use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, ClickEvent, ElementId, InteractiveElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window, px, rgb,
};
use gpui_component::{
    Disableable,
    button::{Button, ButtonVariant, ButtonVariants},
};

/// Control heights on the 4 px grid (polish plan 2.3): chips 24, buttons and inputs 32,
/// list rows 40.
pub const CHIP_HEIGHT: f32 = 24.;
pub const CONTROL_HEIGHT: f32 = 32.;
pub const ROW_HEIGHT: f32 = 40.;
/// Controls get a 2 px radius. Overlays take 4 px. List rows and panels stay square.
pub const CONTROL_RADIUS: f32 = 2.;
pub const OVERLAY_RADIUS: f32 = 4.;
/// Spacing steps on the 4 px grid.
pub const SPACE_XS: f32 = 4.;
pub const SPACE_S: f32 = 8.;
pub const SPACE_M: f32 = 12.;

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
        .h(px(CONTROL_HEIGHT))
        .rounded(px(CONTROL_RADIUS))
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
        .h(px(CONTROL_HEIGHT))
        .rounded(px(CONTROL_RADIUS))
        .text_color(rgb(p.muted))
        .disabled(!enabled)
        .tab_stop(enabled)
        .on_click(click);
    crate::platform::layout_probe::mark(button, &id, &label, enabled)
}

/// The one chip (polish plan 3.1 findings 5 and 6): a 24 px control with a pixel label
/// and an optional value. `tooltip` carries the full text when the value is truncated.
pub fn chip(
    p: Palette,
    id: impl Into<ElementId>,
    label: &'static str,
    value: impl Into<SharedString>,
    active: bool,
    enabled: bool,
    click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    let value: SharedString = value.into();
    Button::new(id)
        .ghost()
        .h(px(CHIP_HEIGHT))
        .px(px(SPACE_S))
        .rounded(px(CONTROL_RADIUS))
        .border_1()
        .border_color(rgb(if active { p.accent } else { p.line }))
        .text_color(rgb(p.muted))
        .disabled(!enabled)
        .tab_stop(enabled)
        .on_click(click)
        .child(
            row()
                .gap(px(SPACE_XS))
                .child(
                    gpui::div()
                        .flex_shrink_0()
                        .font_family(LABEL_FONT)
                        .text_size(p.text(Step::Label))
                        .text_color(rgb(p.muted))
                        .child(label),
                )
                .when(!value.is_empty(), |d| {
                    d.child(
                        gpui::div()
                            .min_w_0()
                            .truncate()
                            .text_size(p.text(Step::Ui))
                            .text_color(rgb(p.fg))
                            .child(value),
                    )
                }),
        )
}

/// A text input on the control grid: 32 px tall with the control radius.
pub fn input(editor: &gpui::Entity<gpui_component::input::InputState>) -> gpui::Div {
    gpui::div()
        .h(px(CONTROL_HEIGHT))
        .rounded(px(CONTROL_RADIUS))
        .overflow_hidden()
        .child(gpui_component::input::Input::new(editor))
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
        .text_size(p.text(Step::Meta))
        .text_color(rgb(p.muted))
        .child(label.into())
}
/// A dismissable error notice: error-coloured text, a 1 px error rule and a close control.
pub fn banner(
    p: Palette,
    id: &'static str,
    dismiss_id: &'static str,
    message: impl Into<SharedString>,
    dismiss: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    row()
        .id(id)
        .gap(px(SPACE_S))
        .p(px(SPACE_S))
        .border_1()
        .border_color(rgb(p.error))
        .bg(rgb(p.panel))
        .child(
            gpui::div()
                .flex_1()
                .min_w_0()
                .text_size(p.text(Step::Meta))
                .text_color(rgb(p.error))
                .child(message.into()),
        )
        .child(icon_button(p, dismiss_id, "×", "Dismiss", dismiss))
}

/// Fixed-width leading column for starred rows, so names line up whether or not they are starred.
pub fn star_column(p: Palette, starred: bool) -> gpui::Div {
    gpui::div()
        .w(px(16.))
        .flex_shrink_0()
        .text_color(rgb(p.accent))
        .child(if starred { "★" } else { "" })
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
        .gap(px(SPACE_S))
        .p(px(SPACE_M))
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
            .h(px(ROW_HEIGHT))
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
                    .text_size(p.text(Step::Ui))
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
        col().h(px(ROW_HEIGHT)).pr_2().justify_center().child(
            Button::new(id)
                .ghost()
                .tab_stop(true)
                .w_full()
                .p_0()
                .h(px(ROW_HEIGHT))
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
