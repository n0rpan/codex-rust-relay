//! Shared terminal model selection for fresh Codex and Claude Code launches.

use std::collections::BTreeSet;
use std::io::{BufRead, Write};

use crate::clients::ClientKind;
use crate::provider::RelayError;

const PAGE_SIZE: usize = 12;

pub(crate) enum LaunchChoice<'a> {
    Model(&'a str),
    Subscription,
}

pub(crate) fn choose_model<'a>(
    client: ClientKind,
    models: &[(&'a str, &'a str)],
    signed_in: bool,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
) -> Result<Option<LaunchChoice<'a>>, RelayError> {
    match models {
        [] if !signed_in => return Err("no models are ready to launch. Run `ox doctor` to check providers, or supply a known alias with `--model`".into()),
        [(alias, _)] if !signed_in => return Ok(Some(LaunchChoice::Model(alias))),
        _ => {}
    }

    let (client, plan) = match client {
        ClientKind::Codex => ("Codex", "ChatGPT"),
        ClientKind::Claude => ("Claude Code", "Claude"),
    };
    let providers: BTreeSet<_> = models.iter().map(|(_, provider)| *provider).collect();
    writeln!(
        output,
        "Choose how to start {client} ({} API models available)",
        models.len()
    )?;
    writeln!(
        output,
        "Providers: {}",
        providers.into_iter().collect::<Vec<_>>().join(", ")
    )?;
    writeln!(
        output,
        "Type to filter by model or provider. Use :n or :p to page, :all to reset, and :q to cancel."
    )?;

    let mut matches = models.to_vec();
    let mut page = 0;
    let mut answer = String::new();
    loop {
        if signed_in {
            writeln!(output, "   0. {plan} subscription  [native]")?;
        }
        let start = page * PAGE_SIZE;
        let end = (start + PAGE_SIZE).min(matches.len());
        for (index, (alias, provider)) in matches[start..end].iter().enumerate() {
            writeln!(output, "  {:>2}. {alias}  [{provider}]", index + 1)?;
        }
        writeln!(
            output,
            "Showing {}–{} of {}",
            if end == 0 { 0 } else { start + 1 },
            end,
            matches.len()
        )?;
        write!(output, "Model number or search (Enter for first): ")?;
        output.flush()?;
        answer.clear();
        if input.read_line(&mut answer)? == 0 {
            return Ok(None);
        }

        let choice = answer.trim();
        match choice {
            ":q" => return Ok(None),
            "0" | ":native" if signed_in => return Ok(Some(LaunchChoice::Subscription)),
            ":n" => page += usize::from(end < matches.len()),
            ":p" => page = page.saturating_sub(1),
            ":all" => {
                matches = models.to_vec();
                page = 0;
            }
            "" => {
                if let Some((alias, _)) = matches.get(start) {
                    return Ok(Some(LaunchChoice::Model(alias)));
                }
            }
            _ => {
                if let Some((alias, _)) = models.iter().find(|(alias, _)| *alias == choice) {
                    return Ok(Some(LaunchChoice::Model(alias)));
                }
                if let Ok(number) = choice.parse::<usize>() {
                    if let Some((alias, _)) = number
                        .checked_sub(1)
                        .and_then(|index| matches[start..end].get(index))
                    {
                        return Ok(Some(LaunchChoice::Model(alias)));
                    }
                    writeln!(output, "Choose a number from the current page.")?;
                } else {
                    let query = choice.to_ascii_lowercase();
                    matches = models
                        .iter()
                        .copied()
                        .filter(|(alias, provider)| {
                            alias.to_ascii_lowercase().contains(&query)
                                || provider.to_ascii_lowercase().contains(&query)
                        })
                        .collect();
                    page = 0;
                    if matches.is_empty() {
                        writeln!(output, "No matching models. Try another search or :all.")?;
                    }
                }
            }
        }
    }
}
