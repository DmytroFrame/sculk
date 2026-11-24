//! An S3 bucket (or anything that speaks its API: MinIO, Garage, R2...) as the archive of cold regions
//! (docs/docs/07-anvildb.md). Path-style requests signed with AWS Signature Version 4.

use anyhow::{Context, bail};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
pub struct S3 {
    /// `https://s3.eu-central-1.amazonaws.com`, `http://minio:9000`...
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// HMAC-SHA256 (RFC 2104).
fn hmac(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| block.iter().map(|b| b ^ byte).collect::<Vec<u8>>();
    let inner = Sha256::new().chain_update(pad(0x36)).chain_update(message).finalize();
    Sha256::new().chain_update(pad(0x5c)).chain_update(inner).finalize().into()
}

/// S3's URI encoding: everything but unreserved characters, and `/` kept in paths.
fn uri_encode(text: &str, keep_slash: bool) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(char::from(b)),
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The `Authorization` header of a request (AWS Signature Version 4). `headers` are lower-case names with their
/// values and include `host`, `x-amz-content-sha256` and `x-amz-date`; `path` is already URI-encoded.
#[allow(clippy::too_many_arguments)]
pub fn authorization(
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    payload_hash: &str,
    amz_date: &str,
    region: &str,
    access_key: &str,
    secret_key: &str,
) -> String {
    let mut headers: Vec<(&str, &str)> = headers.to_vec();
    headers.sort();
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{}\n", v.trim())).collect();
    let signed: Vec<&str> = headers.iter().map(|(k, _)| *k).collect();
    let signed = signed.join(";");
    let canonical = format!("{method}\n{path}\n\n{canonical_headers}\n{signed}\n{payload_hash}");
    let day = &amz_date[..8];
    let scope = format!("{day}/{region}/s3/aws4_request");
    let to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", sha256_hex(canonical.as_bytes()));
    let key = hmac(format!("AWS4{secret_key}").as_bytes(), day.as_bytes());
    let key = hmac(&key, region.as_bytes());
    let key = hmac(&key, b"s3");
    let key = hmac(&key, b"aws4_request");
    let signature = hex::encode(hmac(&key, to_sign.as_bytes()));
    format!("AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed}, Signature={signature}")
}

/// Now, as S3 wants it: `20130524T000000Z`.
fn amz_date() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z", rest / 3600, rest % 3600 / 60, rest % 60)
}

impl S3 {
    fn request(&self, method: &str, key: &str, body: &[u8]) -> anyhow::Result<(u16, Vec<u8>)> {
        let path = format!("/{}/{}", uri_encode(&self.bucket, false), uri_encode(key, true));
        let host = self.endpoint.split("://").nth(1).unwrap_or(&self.endpoint).trim_end_matches('/').to_owned();
        let (date, hash) = (amz_date(), sha256_hex(body));
        let authorization = authorization(
            method,
            &path,
            &[("host", &host), ("x-amz-content-sha256", &hash), ("x-amz-date", &date)],
            &hash,
            &date,
            &self.region,
            &self.access_key,
            &self.secret_key,
        );
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(60)))
            .http_status_as_error(false)
            .build()
            .into();
        let url = format!("{}{path}", self.endpoint.trim_end_matches('/'));
        let mut response = match method {
            "PUT" => agent
                .put(&url)
                .header("x-amz-content-sha256", &hash)
                .header("x-amz-date", &date)
                .header("authorization", &authorization)
                .send(body)?,
            "GET" => agent
                .get(&url)
                .header("x-amz-content-sha256", &hash)
                .header("x-amz-date", &date)
                .header("authorization", &authorization)
                .call()?,
            _ => agent
                .delete(&url)
                .header("x-amz-content-sha256", &hash)
                .header("x-amz-date", &date)
                .header("authorization", &authorization)
                .call()?,
        };
        let status = response.status().as_u16();
        let body = response.body_mut().with_config().limit(1 << 30).read_to_vec()?;
        Ok((status, body))
    }

    pub fn put(&self, key: &str, body: &[u8]) -> anyhow::Result<()> {
        let (status, answer) = self.request("PUT", key, body).with_context(|| format!("S3 PUT {key}"))?;
        if !(200..300).contains(&status) {
            bail!("S3 PUT {key}: {status} {}", String::from_utf8_lossy(&answer));
        }
        Ok(())
    }

    /// The object, or `None` when the bucket has none at `key`.
    pub fn get(&self, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let (status, body) = self.request("GET", key, &[]).with_context(|| format!("S3 GET {key}"))?;
        match status {
            200..=299 => Ok(Some(body)),
            404 => Ok(None),
            _ => bail!("S3 GET {key}: {status} {}", String::from_utf8_lossy(&body)),
        }
    }

    pub fn delete(&self, key: &str) -> anyhow::Result<()> {
        let (status, body) = self.request("DELETE", key, &[]).with_context(|| format!("S3 DELETE {key}"))?;
        if !(200..300).contains(&status) && status != 404 {
            bail!("S3 DELETE {key}: {status} {}", String::from_utf8_lossy(&body));
        }
        Ok(())
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// AWS's own example (S3 docs, "Signature Calculations for the Authorization Header", GET Object).
    #[test]
    fn the_signature_matches_awss_example() {
        let empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let authorization = authorization(
            "GET",
            "/test.txt",
            &[
                ("host", "examplebucket.s3.amazonaws.com"),
                ("range", "bytes=0-9"),
                ("x-amz-content-sha256", empty),
                ("x-amz-date", "20130524T000000Z"),
            ],
            empty,
            "20130524T000000Z",
            "us-east-1",
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        );
        assert_eq!(
            authorization,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn hmac_matches_rfc_4231() {
        // Test case 2.
        assert_eq!(
            hex::encode(hmac(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    /// A stand-in bucket on a local port: keeps PUT bodies, answers GET and DELETE, refuses unsigned requests.
    pub fn fake_bucket() -> String {
        use std::collections::HashMap;
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let mut objects: HashMap<String, Vec<u8>> = HashMap::new();
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let mut parts = line.split_whitespace();
                let (method, path) = (parts.next().unwrap_or("").to_owned(), parts.next().unwrap_or("").to_owned());
                let (mut length, mut signed) = (0usize, false);
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    let header = header.trim_end();
                    if header.is_empty() {
                        break;
                    }
                    let lower = header.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap();
                    }
                    signed |= lower.starts_with("authorization: aws4-hmac-sha256 credential=");
                }
                let mut body = vec![0u8; length];
                reader.read_exact(&mut body).unwrap();
                let (status, answer) = match (signed, method.as_str()) {
                    (false, _) => ("403 Forbidden", Vec::new()),
                    (true, "PUT") => {
                        objects.insert(path, body);
                        ("200 OK", Vec::new())
                    }
                    (true, "GET") => match objects.get(&path) {
                        Some(object) => ("200 OK", object.clone()),
                        None => ("404 Not Found", Vec::new()),
                    },
                    _ => {
                        objects.remove(&path);
                        ("204 No Content", Vec::new())
                    }
                };
                let head =
                    format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", answer.len());
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&answer);
            }
        });
        url
    }

    #[test]
    fn objects_go_to_the_bucket_and_back() {
        let s3 = S3 {
            endpoint: fake_bucket(),
            bucket: "worlds".into(),
            region: "us-east-1".into(),
            access_key: "key".into(),
            secret_key: "secret".into(),
        };
        s3.put("world/r.0.0.mca.gz", b"region").unwrap();
        assert_eq!(s3.get("world/r.0.0.mca.gz").unwrap().unwrap(), b"region");
        s3.delete("world/r.0.0.mca.gz").unwrap();
        assert_eq!(s3.get("world/r.0.0.mca.gz").unwrap(), None);
    }

    #[test]
    fn dates_are_s3s() {
        let date = amz_date();
        assert_eq!(date.len(), 16);
        assert!(date.starts_with("20") && date.ends_with('Z') && date.as_bytes()[8] == b'T');
    }
}
