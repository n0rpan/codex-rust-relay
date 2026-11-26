//! Shared keyboard-accessible controls for the workspace and preferences.
use crate::appearance::{AccentColor, LABEL_FONT, Palette, Step};
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, App, AppContext, ClickEvent, Context, ElementId, InteractiveElement, IntoElement,
    ParentElement, SharedString, StatefulInteractiveElement, Styled, Window, px, rgb,
};
use gpui_component::{
    Disableable,
    button::{Button, ButtonVariant, ButtonVariants},
    menu::{ContextMenuExt, PopupMenu},
};
use std::rc::Rc;

/// Three control heights on the 4 px grid: inline pills 24, secondary and icon
/// controls 28, primary buttons and inputs 32. List rows share the primary height.
pub const CHIP_HEIGHT: f32 = 24.;
pub const ICON_SIZE: f32 = 28.;
pub const CONTROL_HEIGHT: f32 = 32.;
pub const ROW_HEIGHT: f32 = 32.;
/// The sidebar keyline: every sidebar control box starts and ends this far from its edge,
/// and its text sits `SPACE_S` further in.
pub const GUTTER: f32 = 12.;
/// Controls and list rows take 4 px. Overlays and the composer card take 8 px.
pub const CONTROL_RADIUS: f32 = 4.;
pub const OVERLAY_RADIUS: f32 = 8.;
/// Spacing steps on the 4 px grid.
pub const SPACE_XS: f32 = 4.;
pub const SPACE_S: f32 = 8.;
pub const SPACE_M: f32 = 12.;
pub const SPACE_L: f32 = 16.;

/// The two text buttons: `primary` (accent fill, 32 px, at most one per screen) or quiet
/// (a calm raised surface, 28 px, no border).
pub fn button(
    _palette: Palette,
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
        .h(px(if primary { CONTROL_HEIGHT } else { ICON_SIZE }))
        .px(px(if primary { SPACE_M } else { SPACE_S + 2. }))
        .rounded(px(CONTROL_RADIUS))
        .disabled(!enabled)
        .tab_stop(enabled)
        .on_click(click);
    crate::platform::layout_probe::mark(button, &id, &label, enabled)
}

/// A destructive or high-risk action: a quiet button with error-coloured text.
pub fn danger_button(
    palette: Palette,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    enabled: bool,
    click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    button(palette, id, label, false, enabled, click).text_color(rgb(palette.error))
}

/// A ghost control: no fill until hovered. Retains Button's tab stop, accessible label,
/// and activation behavior.
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
        .h(px(ICON_SIZE))
        .px(px(SPACE_S))
        .rounded(px(CONTROL_RADIUS))
        .text_color(rgb(p.muted))
        .disabled(!enabled)
        .tab_stop(enabled)
        .on_click(click);
    crate::platform::layout_probe::mark(button, &id, &label, enabled)
}

/// The one chip: a borderless 24 px ghost pill showing a value and a caret. A non-empty
/// `label` is a quiet prefix for values that need one ("Effort", "Speed"). `tooltip`
/// carries the full text when the value is truncated.
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
    let id: ElementId = id.into();
    let probe: SharedString = match (label.is_empty(), value.is_empty()) {
        (_, true) => label.into(),
        (true, false) => value.clone(),
        (false, false) => format!("{label} {value}").into(),
    };
    let button = Button::new(id.clone())
        .ghost()
        .h(px(CHIP_HEIGHT))
        .px(px(SPACE_S))
        .rounded(px(CONTROL_RADIUS))
        .when(active, |b| b.bg(rgb(p.hover)))
        .text_color(rgb(p.muted))
        .disabled(!enabled)
        .tab_stop(enabled)
        .accessibility_label(probe.clone())
        .on_click(click)
        .child(
            row()
                .gap(px(SPACE_XS))
                .min_w_0()
                .when(!label.is_empty(), |r| {
                    r.child(
                        gpui::div()
                            .flex_shrink_0()
                            .text_size(p.text(Step::Meta))
                            .text_color(rgb(p.quiet()))
                            .child(label),
                    )
                })
                .when(!value.is_empty(), |d| {
                    d.child(
                        gpui::div()
                            .min_w_0()
                            .truncate()
                            .text_size(p.text(Step::Meta))
                            .text_color(rgb(p.muted))
                            .child(value),
                    )
                })
                .child(caret(p)),
        );
    crate::platform::layout_probe::mark(button, &id, &probe, enabled)
}

/// The quiet ▾ that marks a ghost control as a picker.
pub fn caret(p: Palette) -> gpui::Div {
    gpui::div()
        .flex_shrink_0()
        .text_size(p.text(Step::Label))
        .text_color(rgb(p.quiet()))
        .child("▾")
}

/// A tiny pixel-font caps label ("PROJECT", "AGENT") in the quiet text tier.
pub fn micro_label(p: Palette, text: impl Into<SharedString>) -> gpui::Div {
    gpui::div()
        .flex_shrink_0()
        .font_family(LABEL_FONT)
        .text_size(p.text(Step::Label) * 0.84)
        .text_color(rgb(p.quiet()))
        .child(text.into())
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

/// A menu item the layout probe can find and press by its label. Without the probe it
/// is the plain label item.
pub fn menu_item(label: impl Into<SharedString>) -> PopupMenuItem {
    let label: SharedString = label.into();
    if !crate::platform::layout_probe::active() {
        return PopupMenuItem::new(label);
    }
    let id: ElementId = SharedString::from(format!("menu-item {label}")).into();
    PopupMenuItem::element(move |_, _| {
        crate::platform::layout_probe::mark_element(
            gpui::div().child(label.clone()),
            &id,
            &label,
            true,
        )
    })
}

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
/// A 28 px square ghost control showing a text glyph. `label` is its tooltip, its
/// accessible name and its layout-probe label.
pub fn icon_button(
    p: Palette,
    id: impl Into<ElementId>,
    glyph: &'static str,
    label: impl Into<SharedString>,
    click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    let (id, label) = (id.into(), label.into());
    let button = icon_base(p, id.clone(), label.clone(), true, click).label(glyph);
    crate::platform::layout_probe::mark(button, &id, &label, true)
}

/// A 28 px square ghost control showing a line icon. `label` is its tooltip, its
/// accessible name and its layout-probe label.
pub fn icon(
    p: Palette,
    id: impl Into<ElementId>,
    icon: gpui_component::IconName,
    label: impl Into<SharedString>,
    enabled: bool,
    click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    let (id, label) = (id.into(), label.into());
    let button = icon_base(p, id.clone(), label.clone(), enabled, click).icon(icon);
    crate::platform::layout_probe::mark(button, &id, &label, enabled)
}

fn icon_base(
    p: Palette,
    id: ElementId,
    label: SharedString,
    enabled: bool,
    click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    Button::new(id)
        .ghost()
        .size(px(ICON_SIZE))
        .p_0()
        .rounded(px(CONTROL_RADIUS))
        .text_color(rgb(p.muted))
        .accessibility_label(label.clone())
        .tooltip(label)
        .disabled(!enabled)
        .tab_stop(enabled)
        .on_click(click)
}

/// A full-width quiet row control for the sidebar: a ghost button whose content starts
/// on the text keyline. Children supply the content.
pub fn nav_row(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    height: f32,
    enabled: bool,
    click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    let (id, label) = (id.into(), label.into());
    let button = Button::new(id.clone())
        .ghost()
        .w_full()
        .h(px(height))
        .px(px(SPACE_S))
        .justify_start()
        .rounded(px(CONTROL_RADIUS))
        .accessibility_label(label.clone())
        .disabled(!enabled)
        .tab_stop(enabled)
        .on_click(click);
    crate::platform::layout_probe::mark(button, &id, &label, enabled)
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

pub type RowMenuBuilder = Rc<dyn Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu>;

pub struct ListRow {
    pub cartridge: crate::activity::Cartridge,
    pub title: String,
    pub route: String,
    pub age: String,
    pub status: Option<&'static str>,
    pub selected: bool,
    /// Nesting under a parent session; indents the content by one step per level.
    pub depth: usize,
    /// Whether this row is in the Pinned section.
    pub pinned: bool,
    /// Optional custom arcade colour stripe on the left edge.
    pub accent: Option<AccentColor>,
    /// Short project tag shown when a row lives in Pinned or a cross-project user section.
    pub project_tag: Option<String>,
}
impl Default for ListRow {
    fn default() -> Self {
        Self {
            cartridge: crate::activity::Cartridge::Codex,
            title: String::new(),
            route: String::new(),
            age: String::new(),
            status: None,
            selected: false,
            depth: 0,
            pinned: false,
            accent: None,
            project_tag: None,
        }
    }
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
    /// A 32 px row: muted pixel glyph, title, an exception marker and a quiet age. Only
    /// exceptions (running, needs you, error, unread) get a marker; the rest of the
    /// status (such as "Ended") lives in the tooltip.
    /// Renders the row with an optional context menu (available on right-click and via an
    /// explicit `⋯` action button on the trailing edge).
    pub fn render_with_menu(
        self,
        p: Palette,
        id: impl Into<ElementId>,
        menu_builder: Option<RowMenuBuilder>,
        click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> gpui::Div {
        use gpui::prelude::FluentBuilder;
        let hint = format!("{}\n{}", self.title, self.secondary());
        let name = SharedString::from(self.title.clone());
        let running = self.status == Some("Running");
        let marker = match self.status {
            Some("Needs you") => Some(p.warning),
            Some("Error") => Some(p.error),
            Some("Running") => Some(p.success),
            Some("Unread") => Some(p.accent),
            _ => None,
        };
        let stripe_color = self.accent.map(|c| p.thread_accent(c));
        let indent = self.depth.min(4) as f32 * SPACE_M;
        let track = row()
            .relative()
            .w_full()
            .h(px(ROW_HEIGHT))
            .gap(px(SPACE_S))
            .pl(px(indent))
            .when(menu_builder.is_some(), |track| track.pr(px(20.)))
            .when_some(stripe_color, |track, color| {
                track.child(
                    gpui::div()
                        .absolute()
                        .left(px(0.))
                        .top(px(6.))
                        .bottom(px(6.))
                        .w(px(2.))
                        .rounded(px(1.))
                        .bg(rgb(color)),
                )
            })
            .when(self.depth > 0, |track| {
                track.child(
                    gpui::div()
                        .absolute()
                        .left(px(indent - SPACE_S))
                        .top_0()
                        .h(px(ROW_HEIGHT / 2.0))
                        .w(px(SPACE_S - 2.0))
                        .border_l_1()
                        .border_b_1()
                        .border_color(rgb(p.line)),
                )
            })
            .child(crate::activity::cartridge(
                p,
                self.cartridge,
                self.selected || running,
            ))
            .child(
                gpui::div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(p.text(Step::Ui))
                    .text_color(rgb(if self.selected { p.fg } else { p.muted }))
                    .when(self.selected, |d| d.font_weight(gpui::FontWeight::MEDIUM))
                    .child(self.title),
            )
            .when(self.pinned, |d| {
                d.child(
                    gpui::div()
                        .flex_shrink_0()
                        .font_family(LABEL_FONT)
                        .text_size(p.text(Step::Label) * 0.9)
                        .text_color(rgb(p.accent))
                        .child("★"),
                )
            })
            .when_some(self.project_tag, |d, tag| {
                d.child(
                    gpui::div()
                        .flex_shrink_0()
                        .max_w(px(64.))
                        .truncate()
                        .px(px(4.))
                        .py(px(1.))
                        .rounded(px(2.))
                        .bg(rgb(p.surface()))
                        .font_family(LABEL_FONT)
                        .text_size(p.text(Step::Label) * 0.85)
                        .text_color(rgb(p.quiet()))
                        .child(tag),
                )
            })
            .when_some(marker, |d, color| {
                d.child(
                    gpui::div()
                        .flex_shrink_0()
                        .size(px(6.))
                        .rounded_full()
                        .bg(rgb(color)),
                )
            })
            .when(!self.age.is_empty(), |d| {
                d.child(
                    gpui::div()
                        .flex_shrink_0()
                        .min_w(px(22.))
                        .flex()
                        .justify_end()
                        .font_family(LABEL_FONT)
                        .text_size(p.text(Step::Label))
                        .text_color(rgb(p.quiet()))
                        .child(self.age),
                )
            });
        let id = id.into();
        let button = Button::new(id.clone())
            .ghost()
            .tab_stop(true)
            .w_full()
            .h(px(ROW_HEIGHT))
            .px(px(SPACE_S))
            .rounded(px(CONTROL_RADIUS))
            .when(self.selected, |b| b.bg(rgb(p.surface())))
            .child(track)
            .tooltip(hint)
            .on_click(click);
        let button = crate::platform::layout_probe::mark(button, &id, &name, true);
        let action_button: Option<AnyElement> = menu_builder.as_ref().map(|build| {
            let build = build.clone();
            let menu_id = ElementId::Name(format!("row-menu-{id}").into());
            let menu_btn = icon_button(
                p,
                menu_id,
                "⋯",
                format!("Thread actions for {name}"),
                |_, _, _| {},
            )
            .size(px(20.))
            .dropdown_menu(move |menu, window, cx| build(menu, window, cx));
            gpui::div()
                .absolute()
                .right(px(4.))
                .top(px((ROW_HEIGHT - 20.) / 2.))
                .child(menu_btn)
                .into_any_element()
        });
        let row_element: AnyElement = match menu_builder {
            Some(build) => button
                .context_menu(move |menu, window, cx| build(menu, window, cx))
                .into_any_element(),
            None => button.into_any_element(),
        };
        col()
            .relative()
            .h(px(ROW_HEIGHT))
            .justify_center()
            .child(row_element)
            .children(action_button)
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
            depth: 0,
            ..Default::default()
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
