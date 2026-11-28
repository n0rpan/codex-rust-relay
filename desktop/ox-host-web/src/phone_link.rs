//! The same scoped target is used by phone notifications and desktop session links.
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ox_desk_core::{host_protocol::Scope, runtime::Billing};
use sha2::{Digest, Sha256};
use std::io;

pub fn session_target(scope: &Scope, thread: &str) -> io::Result<String> {
    let alias = match &scope.route.billing {
        Billing::Subscription => None,
        Billing::Api(alias) => Some(alias.as_str()),
    };
    let identity = serde_json::to_vec(&(&scope.project, scope.route.runtime, alias, thread))?;
    let hash = URL_SAFE_NO_PAD.encode(Sha256::digest(identity));
    Ok(format!("{thread}~{hash}"))
}

pub fn session_url(url: &str, target: &str) -> io::Result<String> {
    let mut link = url::Url::parse(url).map_err(io::Error::other)?;
    let mut fragment = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in url::form_urlencoded::parse(link.fragment().unwrap_or_default().as_bytes())
    {
        if name != "thread" {
            fragment.append_pair(&name, &value);
        }
    }
    fragment.append_pair("thread", target);
    link.set_fragment(Some(&fragment.finish()));
    Ok(link.to_string())
}

pub fn invitation_link(url: &str, target: &str) -> io::Result<(String, String)> {
    let link = session_url(url, target)?;
    let svg = qrcode::QrCode::new(link.as_str())
        .map_err(io::Error::other)?
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(240, 240)
        .build();
    Ok((link, svg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ox_desk_core::runtime::Route;

    #[test]
    fn invitation_carries_the_same_scoped_target_as_push() {
        let mut scope = Scope {
            project: "/payments".into(),
            route: Route::CODEX_SUBSCRIPTION,
        };
        let target = session_target(&scope, "thread").unwrap();
        scope.route = Route::codex_api("example/model".into());
        assert_ne!(session_target(&scope, "thread").unwrap(), target);
        let (url, svg) =
            invitation_link("https://studio.fixture.ts.net/#pair=fixture", &target).unwrap();
        let url = url::Url::parse(&url).unwrap();
        let args: std::collections::HashMap<_, _> =
            url::form_urlencoded::parse(url.fragment().unwrap().as_bytes()).collect();
        assert_eq!(args.get("pair").unwrap(), "fixture");
        assert_eq!(args.get("thread").unwrap(), &target);
        assert!(svg.contains("<svg"));
    }
}
