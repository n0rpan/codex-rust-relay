//! Shortcut reference shared by the UI and the documentation generator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Palette,
    New,
    Next,
    Previous,
    Composer,
    Find,
    Stop,
    Sidebar,
    Changes,
    Settings,
    Answer(usize),
    SubmitAnswers,
}
pub struct Binding {
    pub chord: &'static str,
    pub command: Command,
    pub description: &'static str,
}
macro_rules! keys {
    ($(($chord:literal,$command:expr,$description:literal)),* $(,)?) => {
        pub const BINDINGS:&[Binding]=&[$(Binding{chord:$chord,command:$command,description:$description}),*];
    }
}
keys![
    (
        "mod-k",
        Command::Palette,
        "Open or close the command palette"
    ),
    (
        "mod-n",
        Command::New,
        "Start a new thread on the current project and billing route"
    ),
    (
        "mod-alt-down",
        Command::Next,
        "Select the next thread in the project list"
    ),
    (
        "mod-alt-up",
        Command::Previous,
        "Select the previous thread in the project list"
    ),
    (
        "mod-f",
        Command::Find,
        "Find text in the current transcript"
    ),
    ("mod-l", Command::Composer, "Focus the current composer"),
    (
        "mod-shift-x",
        Command::Stop,
        "Stop the active turn or selected running worker"
    ),
    (
        "mod-b",
        Command::Sidebar,
        "Show or hide the project sidebar"
    ),
    (
        "mod-shift-d",
        Command::Changes,
        "Show or hide the current thread’s changes and plan"
    ),
    ("mod-,", Command::Settings, "Open or close Settings"),
    (
        "mod-alt-1",
        Command::Answer(0),
        "Choose the first response shown for the visible request"
    ),
    (
        "mod-alt-2",
        Command::Answer(1),
        "Choose the second response shown for the visible request"
    ),
    (
        "mod-alt-3",
        Command::Answer(2),
        "Choose the third response shown for the visible request"
    ),
    (
        "mod-alt-4",
        Command::Answer(3),
        "Choose the fourth response shown for the visible request"
    ),
    (
        "mod-alt-enter",
        Command::SubmitAnswers,
        "Submit the filled answers for the visible questionnaire"
    ),
];
pub fn reference() -> String {
    let mut page = String::from(
        "# Desktop keyboard shortcuts\n\nUse these shortcuts in the active OX Desktop window. Request shortcuts act on the response shown in the request panel.\n\n| Linux | macOS | Action |\n| --- | --- | --- |\n",
    );
    for binding in BINDINGS {
        fn format(chord: &str, modifier: &str) -> String {
            chord
                .split('-')
                .map(|part| match part {
                    "mod" => modifier.into(),
                    "alt" => "Alt".into(),
                    "shift" => "Shift".into(),
                    "down" => "↓".into(),
                    "up" => "↑".into(),
                    "enter" => "Enter".into(),
                    value => value.to_uppercase(),
                })
                .collect::<Vec<_>>()
                .join("+")
        }
        page.push_str(&format!(
            "| {} | {} | {} |\n",
            format(binding.chord, "Ctrl"),
            format(binding.chord, "Cmd"),
            binding.description
        ));
    }
    page.push_str("\nIn the command palette, use ↑ and ↓ to select a result, Enter to open it, and Esc to return to your previous focus.\n");
    page.push_str("\nIn the composer, type `@` and a filename, use ↑ and ↓ to select a match, and Tab to insert it. Paste images with Ctrl+V or Cmd+V. Enter sends the draft. Shift+Enter inserts a new line. Ctrl+Enter or Cmd+Enter also sends. In Settings → Appearance & input, choose Ctrl / Cmd + Enter if you prefer plain Enter to insert a new line.\n");
    page
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bindings_have_a_unique_chord_and_match_the_reference() {
        let chords: std::collections::HashSet<_> = BINDINGS.iter().map(|b| b.chord).collect();
        assert_eq!(chords.len(), BINDINGS.len());
        assert_eq!(reference(), include_str!("../../../docs/desktop/keys.md"));
    }
}
