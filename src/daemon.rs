//! A persistent, authenticated listener for external clients and reverse proxies.

use std::ffi::OsString;
use std::net::Ipv4Addr;
use std::path::Path;
use std::sync::Arc;

use tokio::net::TcpListener;

use crate::catalog::BundledModels;
use crate::provider::{CodeoxConfig, RelayError, SavedKeys, process_env};
use crate::{App, MAX_REQUEST_BYTES, build_search, serve, upstream_client};

const DEFAULT_PORT: u16 = 8787;
pub(crate) use crate::provider::GATEWAY_KEY_ENV as KEY_ENV;

fn bind_error(port: u16, error: std::io::Error) -> RelayError {
    if error.kind() != std::io::ErrorKind::AddrInUse {
        return error.into();
    }
    let other = if port == u16::MAX { port - 1 } else { port + 1 };
    format!(
        "port {port} is already in use\n  why: another program, possibly another ox serve, is listening on 127.0.0.1:{port}\n  fix: ox serve --port {other}   (then use that port in your client settings)"
    )
    .into()
}

pub(crate) async fn run(args: &[OsString], home: &Path) -> Result<u8, RelayError> {
    let port = match args {
        [] => DEFAULT_PORT,
        [flag] if flag == "--help" || flag == "-h" => {
            println!("Usage: ox serve [--port PORT]");
            println!("Listens on 127.0.0.1 (default port {DEFAULT_PORT}).");
            println!("Set {KEY_ENV} to a private, randomly generated gateway key.");
            println!("Run ox help share for server setup, or ox help desktop for client setup.");
            return Ok(0);
        }
        [flag, port] if flag == "--port" => port
            .to_str()
            .and_then(|port| port.parse::<u16>().ok())
            .filter(|port| *port != 0)
            .ok_or("serve port must be between 1 and 65535")?,
        _ => return Err("usage: ox serve [--port PORT]".into()),
    };
    let key = process_env(KEY_ENV)
        .filter(|key| (32..=256).contains(&key.len()) && key.bytes().all(|b| b.is_ascii_graphic()))
        .ok_or(
            "set OX_GATEWAY_KEY to a random 32–256 character ASCII secret (openssl rand -hex 32)",
        )?;
    let path = CodeoxConfig::path(home);
    if !path.is_file() {
        return Err("no ox provider configured. Run `ox setup` before starting `ox serve`".into());
    }
    let config = CodeoxConfig::load(&path)?;
    let saved = SavedKeys::for_config(home, &config)?;
    let env = |name: &str| saved.get(name);
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .await
        .map_err(|error| bind_error(port, error))?;
    let search = config
        .search
        .as_ref()
        .and_then(|settings| build_search(settings, &env));
    let mut gateway = App::codeox(
        config,
        BundledModels::default(),
        &key,
        upstream_client()?,
        MAX_REQUEST_BYTES,
        &env,
        search,
        crate::Catalog::Shared,
    )
    .await?;
    gateway.persistent = true;
    Arc::make_mut(&mut gateway.secrets).push(key);
    eprintln!("ox: serving on http://{}", listener.local_addr()?);
    serve(listener, Arc::new(gateway)).await?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_busy_port_explains_itself_and_offers_another_port() {
        let busy = std::io::Error::from(std::io::ErrorKind::AddrInUse);
        let message = bind_error(8787, busy).to_string();
        assert!(
            message.starts_with("port 8787 is already in use"),
            "{message}"
        );
        assert!(
            message.contains("\n  fix: ox serve --port 8788"),
            "{message}"
        );
        let busy = std::io::Error::from(std::io::ErrorKind::AddrInUse);
        assert!(
            bind_error(u16::MAX, busy)
                .to_string()
                .contains("--port 65534")
        );
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(!bind_error(80, denied).to_string().contains("fix:"));
    }
}
