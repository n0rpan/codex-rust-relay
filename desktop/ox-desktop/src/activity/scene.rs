//! What the agent is doing, as a scene: the pure mapping from a live transcript item to a
//! scene, its plain-words label and its pixel sprite. Nothing here touches the window.
use ox_desk_core::transcript::item::Item;

/// Sprites are drawn on a 20 by 11 grid of two-pixel cells.
pub const GRID: (i32, i32) = (20, 11);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Scene {
    Loading,
    Thinking,
    Typing,
    Reading,
    Building,
    Terminal,
    Testing,
    Web,
    Tool,
    Worker,
    Waiting,
    Done,
    GameOver,
    Fire,
}

/// A palette role. The painter resolves it against the active theme.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ink {
    Fg,
    Accent,
    OnAccent,
    Warning,
    Success,
    Error,
    Line,
    Panel,
}

impl Scene {
    #[cfg(test)]
    pub const ALL: [Self; 14] = [
        Self::Loading,
        Self::Thinking,
        Self::Typing,
        Self::Reading,
        Self::Building,
        Self::Terminal,
        Self::Testing,
        Self::Web,
        Self::Tool,
        Self::Worker,
        Self::Waiting,
        Self::Done,
        Self::GameOver,
        Self::Fire,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Loading => "Loading history…",
            Self::Thinking => "Thinking",
            Self::Typing => "Writing",
            Self::Reading => "Reading files",
            Self::Building => "Building",
            Self::Terminal => "Running a command",
            Self::Testing => "Running tests",
            Self::Web => "Searching the web",
            Self::Tool => "Using a tool",
            Self::Worker => "Delegating to a worker",
            Self::Waiting => "Waiting for you · INSERT COIN",
            Self::Done => "Done · HIGH SCORE",
            Self::GameOver => "GAME OVER · turn failed",
            Self::Fire => "YOLO",
        }
    }

    /// Milliseconds per frame. Waiting can last for hours, so it only blinks.
    pub fn tick_ms(self) -> u64 {
        if self == Self::Waiting { 500 } else { 125 }
    }

    /// Frames a one-shot scene plays before it leaves the slot. None loops.
    pub fn one_shot(self) -> Option<u8> {
        match self {
            Self::Done => Some(16),
            Self::GameOver => Some(24),
            _ => None,
        }
    }

    /// The most telling frame, shown alone when motion is off.
    pub fn key_frame(self) -> u8 {
        match self {
            Self::Building | Self::Testing | Self::Web | Self::Worker => 7,
            Self::Thinking => 4,
            _ => 0,
        }
    }

    /// The scene for the newest item of the running turn.
    pub fn for_item(item: Option<&Item>) -> Self {
        let live =
            |status: &str| matches!(status, "inProgress" | "in_progress" | "running" | "started");
        match item {
            Some(Item::AgentMessage { .. } | Item::Plan { .. }) => Self::Typing,
            Some(Item::AgentOperation { status, .. }) if live(status) => Self::Worker,
            Some(Item::CommandExecution {
                status, command, ..
            }) if live(status) => for_command(command),
            Some(Item::FileChange { status, .. }) if live(status) => Self::Building,
            Some(Item::McpToolCall { status, tool, .. } | Item::ToolCall { status, tool })
                if live(status) =>
            {
                for_tool(tool)
            }
            Some(Item::WebSearch { .. }) => Self::Web,
            _ => Self::Thinking,
        }
    }
}

/// Claude and Codex tool names. Anything unknown is the generic tool scene.
pub fn for_tool(tool: &str) -> Scene {
    match tool.to_ascii_lowercase().as_str() {
        "read" | "glob" | "grep" | "ls" | "notebookread" | "read_file" | "list_dir" | "view" => {
            Scene::Reading
        }
        "edit" | "write" | "multiedit" | "notebookedit" | "apply_patch" | "file_change" => {
            Scene::Building
        }
        "bash" | "bashoutput" | "powershell" | "shell" | "exec_command" | "command_execution" => {
            Scene::Terminal
        }
        "websearch" | "webfetch" | "web_search" | "web_fetch" => Scene::Web,
        "task" | "agent" | "workflow" | "spawn_agent" => Scene::Worker,
        _ => Scene::Tool,
    }
}

/// A shell command: a test run anywhere in it wins, then the first program decides.
pub fn for_command(command: &str) -> Scene {
    let words: Vec<&str> = command
        .split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ';' | '&' | '|' | '(' | ')'))
        .filter(|w| !w.is_empty())
        .map(|w| w.rsplit('/').next().unwrap_or(w))
        .collect();
    let runs_tests = words.iter().enumerate().any(|(i, word)| {
        let next = words.get(i + 1).copied().unwrap_or_default();
        let after = words.get(i + 2).copied().unwrap_or_default();
        match *word {
            "pytest" | "nextest" => true,
            "cargo" | "go" | "make" | "just" => next == "test" || next == "nextest",
            "npm" | "pnpm" | "yarn" | "bun" => next == "test" || (next == "run" && after == "test"),
            _ => false,
        }
    });
    if runs_tests {
        return Scene::Testing;
    }
    // Codex wraps commands as `bash -lc "…"`: skip the shell and its flags.
    // A leading `cd dir`, flags and `KEY=value` words are not the program either.
    let mut rest = words.iter().copied();
    let program = loop {
        match rest.next() {
            Some("cd") => drop(rest.next()),
            Some("bash" | "zsh" | "sh" | "env" | "sudo") => {}
            Some(word) if word.starts_with('-') || word.contains('=') => {}
            other => break other.unwrap_or_default(),
        }
    };
    match program {
        "cat" | "rg" | "grep" | "ls" | "find" | "fd" | "head" | "tail" | "tree" | "wc" | "less"
        | "bat" => Scene::Reading,
        "apply_patch" => Scene::Building,
        "curl" | "wget" => Scene::Web,
        _ => Scene::Terminal,
    }
}

/// A stable seed for one turn of one thread (FNV-1a), so a re-render never reshuffles.
pub fn seed(thread: &str, turn: &str) -> u64 {
    thread
        .bytes()
        .chain([0])
        .chain(turn.bytes())
        .fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
}

pub const THINKING_VARIANTS: u64 = 4;
const VARIANT_SECONDS: u64 = 12;

/// Which thinking sprite to show: the turn picks the first, the clock moves on every 12 s.
pub fn thinking_variant(seed: u64, seconds: u64) -> u8 {
    ((seed % THINKING_VARIANTS + seconds / VARIANT_SECONDS) % THINKING_VARIANTS) as u8
}

/// The front-facing mini ox: seven wide, from two above `y` to four below.
fn face(r: &mut impl FnMut(i32, i32, i32, i32, Ink), x: i32, y: i32) {
    r(x, y, 7, 4, Ink::Accent);
    for dx in [1, 5] {
        r(x + dx, y - 2, 1, 2, Ink::Fg);
        r(x + dx, y + 1, 1, 1, Ink::OnAccent);
        r(x + dx, y + 4, 1, 1, Ink::Accent);
    }
}

/// Emit the rectangles of one frame as (x, y, w, h, ink) in grid cells.
/// Loading and Fire are full-size ox sprites and are drawn by the painter itself.
pub fn draw(scene: Scene, variant: u8, frame: u8, r: &mut impl FnMut(i32, i32, i32, i32, Ink)) {
    use Ink::*;
    let b = i32::from(frame % 8);
    match scene {
        Scene::Thinking => match variant % THINKING_VARIANTS as u8 {
            // A thought bubble counting its dots.
            0 => {
                r(3, 2, 13, 5, Accent);
                r(2, 3, 1, 3, Accent);
                r(16, 3, 1, 3, Accent);
                r(5, 7, 3, 1, Accent);
                r(4, 8, 1, 1, Accent);
                for i in 0..3 {
                    r(
                        5 + i * 4,
                        4,
                        2,
                        1,
                        if b / 2 % 3 == i { Fg } else { OnAccent },
                    );
                }
            }
            // A rally of Pong while the answer loads.
            1 => {
                const BALL: [(i32, i32); 8] = [
                    (3, 4),
                    (7, 6),
                    (11, 8),
                    (15, 6),
                    (16, 4),
                    (12, 2),
                    (8, 4),
                    (4, 6),
                ];
                let (x, y) = BALL[b as usize];
                let follow = (y - 1).clamp(0, 7);
                r(1, if x < 10 { follow } else { 3 }, 1, 4, Fg);
                r(18, if x >= 10 { follow } else { 3 }, 1, 4, Fg);
                for i in 0..5 {
                    r(10, 1 + i * 2, 1, 1, Line);
                }
                r(x, y, 1, 1, Warning);
            }
            // The light bulb: dim, then the idea lands.
            2 => {
                let lit = b >= 3;
                let glass = if lit { Warning } else { Line };
                r(8, 1, 5, 1, glass);
                r(7, 2, 7, 4, glass);
                r(8, 6, 5, 1, glass);
                r(9, 4, 3, 1, if lit { Fg } else { Panel });
                r(9, 7, 3, 2, Accent);
                r(10, 9, 1, 1, Accent);
                if lit {
                    let reach = b % 2;
                    r(4 - reach, 3, 2, 1, Warning);
                    r(15 + reach, 3, 2, 1, Warning);
                    r(5, 0, 1, 1, Warning);
                    r(15, 0, 1, 1, Warning);
                }
            }
            // The ox ponders: dots rise to a question mark.
            _ => {
                face(r, 2, 4);
                if b >= 1 {
                    r(10, 6, 1, 1, Fg);
                }
                if b >= 2 {
                    r(12, 4, 1, 1, Fg);
                }
                if b >= 3 {
                    let ink = if b % 2 == 0 { Warning } else { Fg };
                    r(14, 1, 3, 1, ink);
                    r(16, 2, 1, 2, ink);
                    r(15, 4, 1, 2, ink);
                    r(15, 7, 1, 1, ink);
                }
            }
        },
        Scene::Typing | Scene::Tool => {
            r(2, 1, 15, 8, Accent);
            r(3, 2, 13, 6, Panel);
            r(4, 3, 1, 1, Fg);
            r(5, 4, 1, 1, Fg);
            r(4, 5, 1, 1, Fg);
            if scene == Scene::Typing {
                r(7, 5, 1 + b, 1, Accent);
            } else {
                r(7 + b / 2, 6, 3, 1, Warning);
            }
            r(6, 9, 7, 1, Line);
        }
        // The ox reads: the highlighted line moves down both pages, then a page turns.
        Scene::Reading => {
            face(r, 1, 4);
            r(10, 3, 4, 6, Fg);
            r(15, 3, 4, 6, Fg);
            r(14, 4, 1, 6, Line);
            r(10, 9, 9, 1, Line);
            for i in 0..4 {
                let ink = if b / 2 == i { Accent } else { Line };
                r(11 + (i / 2) * 5, 4 + (i % 2) * 2, 2, 1, ink);
            }
            if b == 7 {
                r(14, 1, 1, 3, Fg);
                r(13, 2, 1, 1, Fg);
            }
        }
        // Bricks drop into a wall, one per frame.
        Scene::Building => {
            const SLOTS: [(i32, i32); 8] = [
                (2, 8),
                (7, 8),
                (12, 8),
                (4, 5),
                (9, 5),
                (14, 5),
                (2, 2),
                (7, 2),
            ];
            r(1, 10, 18, 1, Line);
            for (i, (x, y)) in SLOTS.iter().enumerate() {
                match (i as i32).cmp(&b) {
                    std::cmp::Ordering::Less => r(*x, *y, 4, 2, Accent),
                    std::cmp::Ordering::Equal => r(*x, *y - 1, 4, 2, Warning),
                    std::cmp::Ordering::Greater => {}
                }
            }
        }
        // A cabinet with a prompt, a blinking cursor and one key going down.
        Scene::Terminal => {
            r(2, 0, 15, 8, Accent);
            r(3, 1, 13, 6, Panel);
            r(4, 2, 1, 1, Fg);
            r(5, 3, 1, 1, Fg);
            r(4, 4, 1, 1, Fg);
            let typed = b.min(6);
            if typed > 0 {
                r(7, 3, typed, 1, Accent);
            }
            if b % 2 == 0 {
                r(7 + typed, 3, 1, 1, Warning);
            }
            r(3, 9, 14, 2, Line);
            r(4 + (b * 5) % 12, 9, 2, 1, Fg);
        }
        // A checkered flag waves while three result lamps turn green.
        Scene::Testing => {
            r(2, 0, 1, 11, Fg);
            for col in 0..5 {
                for row in 0..3 {
                    let ink = if (col + row) % 2 == 0 { Fg } else { Line };
                    r(3 + col * 2, 1 + row * 2 + (col + b / 2) % 2, 2, 2, ink);
                }
            }
            for i in 0..3 {
                r(15, 1 + i * 3, 3, 2, if b / 2 > i { Success } else { Line });
            }
        }
        // A turning globe sends out waves.
        Scene::Web => {
            for (i, half) in [2, 3, 4, 4, 4, 4, 4, 3, 2].iter().enumerate() {
                r(8 - half, 1 + i as i32, half * 2 + 1, 1, Accent);
            }
            r(4, 5, 9, 1, OnAccent);
            r(5 + b * 7 / 8, 2, 1, 7, OnAccent);
            for i in 0..3 {
                if b / 2 > i || b == 7 {
                    r(14 + i * 2, 4 - i, 1, 3 + i * 2, Warning);
                }
            }
        }
        // The ox sends out mini oxen, one more every other frame.
        Scene::Worker => {
            face(r, 1, 3);
            if b % 2 == 1 {
                r(8, 4, 1, 1, Warning);
            }
            for k in 0..3 {
                if b / 2 >= k {
                    let x = 9 + k * 4;
                    let y = 7 + (b + k) % 2;
                    r(x, y, 3, 2, Accent);
                    r(x, y - 1, 1, 1, Fg);
                    r(x + 2, y - 1, 1, 1, Fg);
                }
            }
        }
        // PLAYER 1 blinks while a coin hangs over the slot.
        Scene::Waiting => {
            if b % 2 == 0 {
                r(2, 2, 1, 5, Fg);
                r(1, 3, 1, 1, Fg);
                r(1, 7, 3, 1, Fg);
                r(5, 2, 1, 6, Fg);
                r(6, 2, 2, 1, Fg);
                r(8, 3, 1, 2, Fg);
                r(6, 5, 2, 1, Fg);
            }
            let drop = b % 4;
            r(13, drop, 3, 1, Warning);
            r(12, drop + 1, 5, 2, Warning);
            r(13, drop + 3, 3, 1, Warning);
            r(14, drop + 1, 1, 2, OnAccent);
            r(11, 7, 8, 4, Accent);
            r(13, 8, 4, 1, Panel);
        }
        // A trophy with a travelling shine and four twinkling stars.
        Scene::Done => {
            r(5, 2, 10, 1, Warning);
            r(5, 3, 1, 2, Warning);
            r(14, 3, 1, 2, Warning);
            r(7, 3, 6, 3, Warning);
            r(8, 6, 4, 1, Warning);
            r(9, 7, 2, 2, Warning);
            r(7, 9, 6, 1, Fg);
            r(8 + b % 4, 3, 1, 2, Fg);
            for (i, (x, y)) in [(2, 2), (17, 1), (16, 7), (3, 7)].iter().enumerate() {
                r(*x, *y, 1, 1, Fg);
                if (b + i as i32 * 2) % 4 < 2 {
                    r(*x - 1, *y, 3, 1, Fg);
                    r(*x, *y - 1, 1, 3, Fg);
                }
            }
        }
        // The ox is out: crossed eyes, flashing red.
        Scene::GameOver => {
            let body = if b % 4 < 2 { Error } else { Line };
            r(5, 3, 10, 6, body);
            r(6, 1, 1, 2, Fg);
            r(13, 1, 1, 2, Fg);
            for x in [6, 11] {
                for (dx, dy) in [(0, 0), (2, 0), (1, 1), (0, 2), (2, 2)] {
                    r(x + dx, 4 + dy, 1, 1, Panel);
                }
            }
            r(8, 7, 4, 1, Panel);
            r(4, 10, 12, 1, Line);
        }
        Scene::Loading | Scene::Fire => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(command: &str, status: &str) -> Item {
        Item::CommandExecution {
            cwd: None,
            command: command.into(),
            status: status.into(),
            aggregated_output: None,
            exit_code: None,
        }
    }
    fn tool(tool: &str) -> Item {
        Item::ToolCall {
            status: "running".into(),
            tool: tool.into(),
        }
    }

    #[test]
    fn a_live_item_picks_the_scene_that_says_what_is_happening() {
        for (name, scene) in [
            ("Read", Scene::Reading),
            ("Glob", Scene::Reading),
            ("Grep", Scene::Reading),
            ("MultiEdit", Scene::Building),
            ("apply_patch", Scene::Building),
            ("WebFetch", Scene::Web),
            ("web_search", Scene::Web),
            ("Workflow", Scene::Worker),
            ("SomethingNew", Scene::Tool),
        ] {
            assert_eq!(Scene::for_item(Some(&tool(name))), scene, "{name}");
        }
        for (text, scene) in [
            ("ls -la", Scene::Reading),
            ("/bin/bash -lc \"rg -n Scene desktop\"", Scene::Reading),
            ("cat Cargo.toml | head", Scene::Reading),
            ("cargo build --release", Scene::Terminal),
            ("git status", Scene::Terminal),
            (
                "cd desktop && RUSTFLAGS=-Dwarnings cargo clippy",
                Scene::Terminal,
            ),
            ("cd src && ls", Scene::Reading),
            ("cargo test -q -p ox-desktop", Scene::Testing),
            ("cd desktop && cargo nextest run", Scene::Testing),
            ("bash -lc 'python -m pytest -x'", Scene::Testing),
            ("pnpm run test", Scene::Testing),
            ("yarn test --watch=false", Scene::Testing),
            ("go test ./...", Scene::Testing),
            ("make test", Scene::Testing),
            ("just test", Scene::Testing),
            ("curl -s https://example.com", Scene::Web),
            ("", Scene::Terminal),
        ] {
            assert_eq!(
                Scene::for_item(Some(&command(text, "inProgress"))),
                scene,
                "{text}"
            );
        }
        // A word that merely contains a runner name is not a test run.
        assert_eq!(for_command("cat tests/go.md"), Scene::Reading);
        assert_eq!(for_command("echo cargo testing"), Scene::Terminal);
    }

    #[test]
    fn other_items_keep_their_scene_and_finished_tools_fall_back_to_thinking() {
        let change = |status: &str| Item::FileChange {
            status: status.into(),
            changes: vec![],
        };
        let worker = Item::AgentOperation {
            tool: "explore".into(),
            status: "in_progress".into(),
            sender_thread_id: "a".into(),
            receiver_thread_ids: vec![],
        };
        let search = Item::WebSearch { query: "ox".into() };
        let message = Item::AgentMessage {
            text: "hi".into(),
            is_update: false,
        };
        let mcp = Item::McpToolCall {
            status: "started".into(),
            server: "docs".into(),
            tool: "lookup".into(),
        };
        assert_eq!(
            Scene::for_item(Some(&change("inProgress"))),
            Scene::Building
        );
        assert_eq!(Scene::for_item(Some(&worker)), Scene::Worker);
        assert_eq!(Scene::for_item(Some(&search)), Scene::Web);
        assert_eq!(Scene::for_item(Some(&message)), Scene::Typing);
        assert_eq!(Scene::for_item(Some(&mcp)), Scene::Tool);
        assert_eq!(Scene::for_item(Some(&change("completed"))), Scene::Thinking);
        assert_eq!(
            Scene::for_item(Some(&command("cargo test", "completed"))),
            Scene::Thinking
        );
        assert_eq!(Scene::for_item(None), Scene::Thinking);
    }

    #[test]
    fn thinking_rotates_every_twelve_seconds_from_a_stable_turn_seed() {
        let a = seed("thread-1", "turn-1");
        assert_eq!(a, seed("thread-1", "turn-1"));
        assert_ne!(a, seed("thread-1", "turn-2"));
        assert_ne!(seed("ab", "c"), seed("a", "bc"));
        // Stable inside a window, different in the next, and every variant comes round.
        assert_eq!(thinking_variant(a, 0), thinking_variant(a, 11));
        assert_ne!(thinking_variant(a, 11), thinking_variant(a, 12));
        let mut seen: Vec<u8> = (0..4).map(|i| thinking_variant(a, i * 12)).collect();
        seen.sort_unstable();
        assert_eq!(seen, [0, 1, 2, 3]);
        assert_eq!(thinking_variant(a, 0), thinking_variant(a, 48));
    }

    #[test]
    fn every_frame_of_every_sprite_stays_on_the_grid_and_is_tiny() {
        for scene in Scene::ALL {
            for variant in 0..THINKING_VARIANTS as u8 {
                for frame in 0..8 {
                    let mut count = 0;
                    draw(scene, variant, frame, &mut |x, y, w, h, _| {
                        count += 1;
                        assert!(
                            x >= 0
                                && y >= 0
                                && w > 0
                                && h > 0
                                && x + w <= GRID.0
                                && y + h <= GRID.1,
                            "{scene:?} variant {variant} frame {frame}: {x},{y} {w}x{h}"
                        );
                    });
                    let own_painter = matches!(scene, Scene::Loading | Scene::Fire);
                    assert_eq!(count == 0, own_painter, "{scene:?}");
                    assert!(count <= 40, "{scene:?} draws {count} rects");
                }
            }
            assert!(scene.key_frame() < 8);
            assert!(!scene.label().is_empty());
        }
    }

    #[test]
    fn only_the_finales_are_one_shot_and_waiting_only_blinks() {
        for scene in Scene::ALL {
            let finale = matches!(scene, Scene::Done | Scene::GameOver);
            assert_eq!(scene.one_shot().is_some(), finale, "{scene:?}");
        }
        assert_eq!(Scene::Waiting.tick_ms(), 500);
        assert_eq!(Scene::Thinking.tick_ms(), 125);
    }
}
