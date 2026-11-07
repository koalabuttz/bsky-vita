//! Test-only bootstrap; all subsequent UI, HTTP, worker and rendering code is real.
//! The harness uses a fresh Vita filesystem and a loopback-only fake PDS.
use std::error::Error;
use std::sync::Arc;

use atrium_api::agent::atp_agent::{AtpAgent, AtpSession};
use bsky_auth::{AuthAgent, AuthClient, FileSessionStore, PdsClient, ResolvedIdentity};

pub fn client() -> Result<Option<AuthClient>, Box<dyn Error>> {
    let path = "ux0:/data/BSKY00001/emulator-test.json";
    let config = match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let endpoint = config["endpoint"].as_str().ok_or("fixture endpoint missing")?;
    let addr: std::net::SocketAddr = endpoint
        .strip_prefix("http://")
        .ok_or("fixture endpoint must use HTTP loopback")?
        .parse()?;
    if addr.ip() != std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST) || addr.port() == 0 {
        return Err("fixture endpoint must use 127.0.0.1 with a port".into());
    }
    let session: AtpSession = serde_json::from_value(serde_json::json!({
        "did": "did:plc:aaaaaaaaaaaaaaaaaaaaaaaa",
        "handle": "tester.test",
        "accessJwt": "emulator-test-access",
        "refreshJwt": "emulator-test-refresh"
    }))?;
    let agent = AtpAgent::new(
        PdsClient::new(Arc::new(bsky_net::VitaHttpClient::new()), endpoint),
        FileSessionStore::new(bsky_auth::SESSION_PATH),
    );
    futures::executor::block_on(agent.resume_session(session))?;
    Ok(Some(AuthClient {
        agent: AuthAgent::Password(agent),
        resolved: ResolvedIdentity {
            did: "did:plc:aaaaaaaaaaaaaaaaaaaaaaaa".into(),
            pds: endpoint.into(),
        },
    }))
}
