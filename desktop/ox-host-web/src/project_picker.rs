//! Read-only directory navigation for the paired operator's new-session picker.
use super::{App, DENY, ErrCode};
use crate::bridge;
use axum::{Json, http::StatusCode};
use ox_desk_core::{
    host_protocol::{Client, Scope, Server, VERSION},
    runtime::Route,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::BufReader;

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum ProjectAction {
    BrowseFolders {
        path: Option<PathBuf>,
        after: Option<String>,
    },
    ChooseProject {
        path: PathBuf,
        route: Route,
    },
}
fn folder(path: Option<PathBuf>) -> Result<PathBuf, ErrCode> {
    path.or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .filter(|p| p.is_absolute())
        .and_then(|p| p.canonicalize().ok())
        .filter(|p| p.is_dir())
        .ok_or(StatusCode::BAD_REQUEST)
}
fn listing(path: &Path, after: Option<String>) -> Result<Value, ErrCode> {
    // Keep only the smallest page of names beyond the cursor, even in huge directories.
    let mut names = std::collections::BTreeSet::new();
    for item in std::fs::read_dir(path)
        .map_err(|_| StatusCode::FORBIDDEN)?
        .flatten()
    {
        let Ok(name) = item.file_name().into_string() else {
            continue;
        };
        if after.as_ref().is_some_and(|last| name <= *last) || name.starts_with('.') {
            continue;
        }
        if item
            .file_type()
            .is_ok_and(|kind| kind.is_dir() || kind.is_symlink() && item.path().is_dir())
        {
            names.insert(name);
            if names.len() > 101 {
                names.pop_last();
            }
        }
    }
    let more = names.len() > 100;
    if more {
        names.pop_last();
    }
    Ok(json!({"path":path,"parent":path.parent(),"folders":names,"more":more}))
}

pub(super) async fn apply(
    app: &App,
    id: &str,
    action: ProjectAction,
) -> Result<Json<Value>, ErrCode> {
    let active = app.state.clone();
    let id = id.to_owned();
    let path = match &action {
        ProjectAction::BrowseFolders { path, .. } => path.clone(),
        ProjectAction::ChooseProject { path, .. } => Some(path.clone()),
    };
    let after = match &action {
        ProjectAction::BrowseFolders { after, .. } => Some(after.clone()),
        _ => None,
    };
    let (dir, rows) = tokio::task::spawn_blocking(move || {
        let dir = folder(path)?;
        let rows = after.map(|cursor| listing(&dir, cursor)).transpose()?;
        Ok::<_, ErrCode>((dir, rows))
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)??;
    if !active.lock().unwrap().alive(&id) {
        return Err(DENY);
    }
    if let Some(rows) = rows {
        return Ok(Json(rows));
    }
    let ProjectAction::ChooseProject { route, .. } = action else {
        return Err(StatusCode::BAD_REQUEST);
    };
    let routes = tokio::time::timeout(Duration::from_secs(5), async {
        let mut conn = bridge::connect(&app.host).await?;
        bridge::write(&mut conn, &Client::Hello { version: VERSION }).await?;
        let bytes = bridge::line(&mut BufReader::new(conn), &mut Vec::new()).await?;
        Ok::<_, std::io::Error>(serde_json::from_slice::<Server>(&bytes)?)
    })
    .await
    .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
    .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    if !matches!(routes,Server::Hello{routes,..} if routes.contains(&route))
        || !active.lock().unwrap().alive(&id)
    {
        return Err(DENY);
    }
    let mut known = app.discovered.lock().unwrap();
    let scope = Scope {
        project: dir.clone(),
        route,
    };
    if !known.contains(&scope) && known.len() >= 8192 {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }
    known.insert(scope);
    Ok(Json(json!({"project":dir})))
}
