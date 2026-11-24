//! Online mode (docs/docs/05-io-server.md, "Обов'язки"): IO encrypts the connection and asks Mojang's session
//! server whether the player really joined, as vanilla's `ServerLoginPacketListenerImpl` does. The profile it gets
//! back (UUID, skin and cape) is what workers see, in their own login or in the engine's Velocity forwarding.

use anyhow::{Context, bail};
use horizon_protocol::crypto;
use horizon_protocol::packets::login;
use horizon_protocol::rsa::RsaPrivateKey;
use std::net::IpAddr;

/// Mojang's session server; `SCULK_SESSION_SERVER` points IO at another one (tests, a mock).
const SESSION_SERVER: &str = "https://sessionserver.mojang.com";

/// Who a player is once the login is done.
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub name: String,
    pub uuid: u128,
    pub properties: Vec<login::Property>,
}

impl Profile {
    /// Offline mode: the name the client gave, and vanilla's offline UUID for it.
    pub fn offline(name: &str) -> Self {
        Self { name: name.to_owned(), uuid: crypto::offline_uuid(name), properties: Vec::new() }
    }
}

/// The server's key pair: one per IO process, as vanilla makes one per server start.
pub struct Keys {
    private: RsaPrivateKey,
    public_der: Vec<u8>,
    session_server: String,
}

impl Keys {
    pub fn generate() -> anyhow::Result<Self> {
        let session_server = std::env::var("SCULK_SESSION_SERVER").unwrap_or_else(|_| SESSION_SERVER.into());
        Self::with_session_server(&session_server)
    }

    /// A key pair that asks `session_server` instead of Mojang's.
    pub fn with_session_server(session_server: &str) -> anyhow::Result<Self> {
        // Vanilla's Crypt.generateKeyPair: 1024-bit RSA.
        let private = RsaPrivateKey::generate(1024).map_err(|e| anyhow::anyhow!(e.0))?;
        let public_der = private.public_der();
        Ok(Self { private, public_der, session_server: session_server.trim_end_matches('/').to_owned() })
    }

    /// The encryption request: an empty server ID (as vanilla since 1.7), the public key and a fresh challenge.
    pub fn request(&self) -> anyhow::Result<(login::Hello, [u8; 4])> {
        let mut challenge = [0u8; 4];
        crypto::random_bytes(&mut challenge).map_err(|e| anyhow::anyhow!(e.0))?;
        let hello = login::Hello {
            server_id: String::new(),
            public_key: self.public_der.clone(),
            challenge: challenge.to_vec(),
            should_authenticate: true,
        };
        Ok((hello, challenge))
    }

    /// Opens the client's encryption response: the shared secret, once the challenge checks out.
    pub fn shared_secret(&self, key: &login::Key, challenge: &[u8; 4]) -> anyhow::Result<[u8; 16]> {
        let answered = self.private.decrypt(&key.challenge).map_err(|e| anyhow::anyhow!(e.0))?;
        if answered != challenge {
            bail!("Protocol error: the client answered the wrong challenge");
        }
        let secret = self.private.decrypt(&key.key).map_err(|e| anyhow::anyhow!(e.0))?;
        secret.try_into().map_err(|_| anyhow::anyhow!("Protocol error: the shared secret is not 16 bytes"))
    }

    /// Asks the session server whether `name` joined this server with `secret`: `Ok(None)` when it says no.
    pub async fn has_joined(
        &self,
        name: &str,
        secret: &[u8; 16],
        ip: Option<IpAddr>,
    ) -> anyhow::Result<Option<Profile>> {
        let hash = crypto::server_hash("", secret, &self.public_der);
        let mut url = format!(
            "{}/session/minecraft/hasJoined?username={}&serverId={}",
            self.session_server,
            encode(name),
            encode(&hash)
        );
        if let Some(ip) = ip {
            url.push_str(&format!("&ip={}", encode(&ip.to_string())));
        }
        let body = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<String>> {
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .timeout_global(Some(std::time::Duration::from_secs(10)))
                .http_status_as_error(false)
                .build()
                .into();
            let mut response = agent.get(&url).call().context("asking the session server")?;
            match response.status().as_u16() {
                200 => Ok(Some(response.body_mut().read_to_string()?)),
                204 | 403 | 404 => Ok(None),
                status => bail!("the session server answered {status}"),
            }
        })
        .await??;
        Ok(body.as_deref().and_then(parse_joined))
    }
}

/// URLEncoder.encode, as vanilla builds the query.
fn encode(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'-' | b'*' | b'_' => out.push(char::from(b)),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The session server's profile: `id` (32 hex digits), `name` and `properties`.
pub fn parse_joined(body: &str) -> Option<Profile> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let uuid = u128::from_str_radix(&v.get("id")?.as_str()?.replace('-', ""), 16).ok()?;
    let name = v.get("name")?.as_str()?.to_owned();
    let properties = v
        .get("properties")
        .and_then(|p| p.as_array())
        .into_iter()
        .flatten()
        .filter_map(|p| {
            Some(login::Property {
                name: p.get("name")?.as_str()?.to_owned(),
                value: p.get("value")?.as_str()?.to_owned(),
                signature: p.get("signature").and_then(|s| s.as_str()).map(str::to_owned),
            })
        })
        .collect();
    Some(Profile { name, uuid, properties })
}

#[cfg(test)]
mod tests {
    use super::*;
    use horizon_protocol::rsa::RsaPublicKey;

    #[test]
    fn the_session_servers_profile_is_read() {
        let body = r#"{"id":"069a79f444e94726a5befca90e38aaf5","name":"Notch","properties":[{"name":"textures","value":"e30=","signature":"c2ln"}]}"#;
        let profile = parse_joined(body).unwrap();
        assert_eq!(profile.uuid, 0x069a79f444e94726a5befca90e38aaf5);
        assert_eq!(profile.name, "Notch");
        assert_eq!(profile.properties[0].signature.as_deref(), Some("c2ln"));
        assert_eq!(parse_joined(""), None);
    }

    #[test]
    fn the_shared_secret_comes_back_through_the_key_pair() {
        let keys = Keys::generate().unwrap();
        let (hello, challenge) = keys.request().unwrap();
        let public = RsaPublicKey::from_der(&hello.public_key).unwrap();
        let secret = [9u8; 16];
        let key = login::Key { key: public.encrypt(&secret).unwrap(), challenge: public.encrypt(&challenge).unwrap() };
        assert_eq!(keys.shared_secret(&key, &challenge).unwrap(), secret);
        let wrong = login::Key { challenge: public.encrypt(&[0, 0, 0, 0]).unwrap(), ..key };
        assert!(keys.shared_secret(&wrong, &challenge).is_err());
    }

    #[test]
    fn names_are_url_encoded_as_java_does() {
        assert_eq!(encode("-1a_b"), "-1a_b");
        assert_eq!(encode("a b/c"), "a+b%2Fc");
    }
}
