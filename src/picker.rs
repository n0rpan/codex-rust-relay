//! Shared terminal model selection for fresh Codex and Claude Code launches.

use std::collections::BTreeSet;
use std::io::{self, StdoutLock, Write};
use std::path::Path;

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use crossterm::style::{Attribute, Print, SetAttribute};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, queue};
use unicode_width::UnicodeWidthChar;

use crate::clients::ClientKind;
use crate::provider::RelayError;
use crate::setup::{lock_config, read_optional_text_limited, write_config_atomic};

const FAVORITES_FILE: &str = "ox-favorites.json";
const MAX_SEARCH_CHARS: usize = 256;
const MAX_VISIBLE_MODELS: usize = 12;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum LaunchChoice<'a> {
    Model(&'a str),
    Subscription,
}

struct Row<'a> {
    choice: LaunchChoice<'a>,
    label: &'a str,
    provider: &'a str,
    search: String,
}

impl<'a> Row<'a> {
    fn new(choice: LaunchChoice<'a>, label: &'a str, provider: &'a str) -> Self {
        Self {
            choice,
            label,
            provider,
            search: format!("{label} {provider}").to_lowercase(),
        }
    }

    fn favorite_rank(&self, favorites: &[String]) -> Option<usize> {
        let LaunchChoice::Model(alias) = self.choice else {
            return None;
        };
        favorites.iter().position(|favorite| favorite == alias)
    }
}

pub(crate) fn choose_model<'a>(
    client: ClientKind,
    models: &[(&'a str, &'a str)],
    signed_in: bool,
    home: &Path,
) -> Result<Option<LaunchChoice<'a>>, RelayError> {
    match models {
        [] if !signed_in => return Err("no models are ready to launch\n  why: no configured provider has both a usable key and a model list".into()),
        [] => return Ok(Some(LaunchChoice::Subscription)),
        [(alias, _)] if !signed_in => return Ok(Some(LaunchChoice::Model(alias))),
        _ => {}
    }

    let (client, plan) = match client {
        ClientKind::Codex => ("Codex", "ChatGPT subscription"),
        ClientKind::Claude => ("Claude Code", "Claude subscription"),
    };
    let mut rows = Vec::with_capacity(models.len() + usize::from(signed_in));
    if signed_in {
        rows.push(Row::new(LaunchChoice::Subscription, plan, "native"));
    }
    rows.extend(
        models
            .iter()
            .map(|(alias, provider)| Row::new(LaunchChoice::Model(alias), alias, provider)),
    );

    let (mut favorites, mut status) = match read_favorites(home) {
        Ok(favorites) => (favorites, String::new()),
        Err(error) => (Vec::new(), error.to_string()),
    };
    let mut term = PickerTerminal::open()?;
    let mut query = String::new();
    let mut only_starred = false;
    let mut selected = usize::from(signed_in);
    let mut first = 0;
    let mut next_choice = None;

    loop {
        let query_lower = query.to_lowercase();
        let mut matches: Vec<_> = rows
            .iter()
            .filter(|row| {
                (!only_starred || row.favorite_rank(&favorites).is_some())
                    && query_lower
                        .split_whitespace()
                        .all(|word| row.search.contains(word))
            })
            .collect();
        matches.sort_by_cached_key(|row| match row.choice {
            LaunchChoice::Subscription => (0, 0),
            LaunchChoice::Model(alias) if alias.eq_ignore_ascii_case(query.trim()) => (1, 0),
            _ => match row.favorite_rank(&favorites) {
                Some(rank) => (2, rank),
                None => (3, 0),
            },
        });
        if let Some(choice) = next_choice.take()
            && let Some(index) = matches.iter().position(|row| row.choice == choice)
        {
            selected = index;
        }

        selected = selected.min(matches.len().saturating_sub(1));
        let (width, height) = terminal::size()
            .ok()
            .filter(|(width, height)| *width > 0 && *height > 0)
            .unwrap_or((80, 24));
        let page_size = usize::from(height.saturating_sub(7)).clamp(1, MAX_VISIBLE_MODELS);
        first = first
            .min(selected)
            .min(matches.len().saturating_sub(page_size))
            .max((selected + 1).saturating_sub(page_size));
        let end = (first + page_size).min(matches.len());

        queue!(term.output, MoveTo(0, 0), Clear(ClearType::All))?;
        line(
            &mut term.output,
            width,
            &format!("Choose a model for {client}"),
        )?;
        line(
            &mut term.output,
            width,
            &format!("Search as you type  {query}▏"),
        )?;
        let mode = if only_starred {
            "Starred only"
        } else {
            "All models"
        };
        line(
            &mut term.output,
            width,
            &format!(
                "{mode} · {} matching · {} starred",
                matches.len(),
                favorites.len()
            ),
        )?;
        line(
            &mut term.output,
            width,
            "↑↓ move   Tab star/unstar   Enter launch   Esc close",
        )?;
        line(
            &mut term.output,
            width,
            "Ctrl+F starred only   Ctrl+U clear search   Alt+↑↓ reorder stars",
        )?;
        if matches.is_empty() {
            line(&mut term.output, width, "No matching models")?;
        }
        for (index, row) in matches[first..end].iter().enumerate() {
            let active = index + first == selected;
            let cursor = if active { ">" } else { " " };
            let star = if row.favorite_rank(&favorites).is_some() {
                "★"
            } else {
                " "
            };
            if active {
                queue!(term.output, SetAttribute(Attribute::Reverse))?;
            }
            let mut label = format!("{cursor} {star} {}", row.label);
            if !row
                .label
                .split_once('/')
                .is_some_and(|(prefix, _)| prefix == row.provider)
            {
                label.push_str(&format!("  [{}]", row.provider));
            }
            line(&mut term.output, width, &label)?;
            if active {
                queue!(term.output, SetAttribute(Attribute::Reset))?;
            }
        }
        if !status.is_empty() {
            line(&mut term.output, width, &status)?;
        }
        term.output.flush()?;

        match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                let control = key.modifiers.contains(KeyModifiers::CONTROL);
                let reset_search = match key.code {
                    KeyCode::Esc => return Ok(None),
                    KeyCode::Char('c' | 'd') if control => return Ok(None),
                    KeyCode::Enter => {
                        if let Some(row) = matches.get(selected) {
                            return Ok(Some(row.choice));
                        }
                        false
                    }
                    KeyCode::Up | KeyCode::Down if key.modifiers.contains(KeyModifiers::ALT) => {
                        if let Some(row) = matches.get(selected)
                            && let LaunchChoice::Model(alias) = row.choice
                            && row.favorite_rank(&favorites).is_some()
                        {
                            match update_favorites(home, |favorites| {
                                if let Some(index) =
                                    favorites.iter().position(|model| model == alias)
                                {
                                    let target = if key.code == KeyCode::Up {
                                        index.saturating_sub(1)
                                    } else {
                                        (index + 1).min(favorites.len() - 1)
                                    };
                                    favorites.swap(index, target);
                                }
                            }) {
                                Ok(updated) => {
                                    favorites = updated;
                                    status.clear();
                                    next_choice = Some(row.choice);
                                }
                                Err(error) => status = error.to_string(),
                            }
                        }
                        false
                    }
                    KeyCode::Up => {
                        selected = selected.saturating_sub(1);
                        false
                    }
                    KeyCode::Down => {
                        selected = selected.saturating_add(1);
                        false
                    }
                    KeyCode::PageUp => {
                        selected = selected.saturating_sub(page_size);
                        false
                    }
                    KeyCode::PageDown => {
                        selected = selected.saturating_add(page_size);
                        false
                    }
                    KeyCode::Tab | KeyCode::BackTab => {
                        if let Some(row) = matches.get(selected)
                            && let LaunchChoice::Model(alias) = row.choice
                        {
                            let star = row.favorite_rank(&favorites).is_none();
                            match update_favorites(home, |favorites| {
                                if star {
                                    if !favorites.iter().any(|model| model == alias) {
                                        favorites.push(alias.to_owned());
                                    }
                                } else {
                                    favorites.retain(|model| model != alias);
                                }
                            }) {
                                Ok(updated) => {
                                    favorites = updated;
                                    status.clear();
                                    next_choice = matches
                                        .get(selected + 1)
                                        .or(Some(row))
                                        .map(|row| row.choice);
                                }
                                Err(error) => status = error.to_string(),
                            }
                        }
                        false
                    }
                    KeyCode::Backspace => query.pop().is_some(),
                    KeyCode::Char('u') if control => {
                        query.clear();
                        true
                    }
                    KeyCode::Char('f') if control => {
                        only_starred = !only_starred;
                        true
                    }
                    KeyCode::Char(value)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                            && query.chars().count() < MAX_SEARCH_CHARS
                            && !value.is_control() =>
                    {
                        query.push(value);
                        true
                    }
                    _ => false,
                };
                if reset_search {
                    selected = usize::from(signed_in && query.trim().is_empty() && !only_starred);
                    first = 0;
                }
            }
            Event::Paste(value) => {
                let remaining = MAX_SEARCH_CHARS.saturating_sub(query.chars().count());
                query.extend(
                    value
                        .chars()
                        .filter(|value| !value.is_control())
                        .take(remaining),
                );
                selected = 0;
                first = 0;
            }
            _ => {}
        }
    }
}

pub(crate) fn read_favorites(home: &Path) -> Result<Vec<String>, RelayError> {
    let Some(text) = read_optional_text_limited(&home.join(FAVORITES_FILE), FAVORITES_FILE)? else {
        return Ok(Vec::new());
    };
    let mut favorites: Vec<String> =
        serde_json::from_str(&text).map_err(|_| format!("Could not read {FAVORITES_FILE}."))?;
    let mut seen = BTreeSet::new();
    favorites.retain(|alias| seen.insert(alias.clone()));
    Ok(favorites)
}

fn update_favorites(
    home: &Path,
    edit: impl FnOnce(&mut Vec<String>),
) -> Result<Vec<String>, RelayError> {
    let _lock = lock_config(home)?;
    let mut favorites = read_favorites(home)?;
    edit(&mut favorites);
    write_config_atomic(
        &home.join(FAVORITES_FILE),
        &serde_json::to_string_pretty(&favorites)?,
    )?;
    Ok(favorites)
}

fn line(output: &mut impl Write, width: u16, text: &str) -> io::Result<()> {
    let mut remaining = usize::from(width.saturating_sub(1));
    let visible: String = text
        .chars()
        .filter(|ch| !ch.is_control())
        .map_while(|ch| {
            remaining = remaining.checked_sub(ch.width().unwrap_or(0))?;
            Some(ch)
        })
        .collect();
    queue!(output, Print(visible), Print("\r\n"))
}

struct PickerTerminal {
    output: StdoutLock<'static>,
}

impl PickerTerminal {
    fn open() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut term = Self {
            output: io::stdout().lock(),
        };
        execute!(
            term.output,
            EnterAlternateScreen,
            Hide,
            EnableBracketedPaste
        )?;
        Ok(term)
    }
}

impl Drop for PickerTerminal {
    fn drop(&mut self) {
        let _ = execute!(
            self.output,
            SetAttribute(Attribute::Reset),
            DisableBracketedPaste,
            Show,
            LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    }
}
