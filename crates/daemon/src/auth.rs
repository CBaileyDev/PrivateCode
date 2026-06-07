use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    middleware::Next,
    response::Response,
};
use std::fs;
use std::path::Path;
use uuid::Uuid;

pub fn get_or_create_token(global_data_dir: &Path) -> Result<String, std::io::Error> {
    let token_file = global_data_dir.join("daemon_token");
    if token_file.exists() {
        let token = fs::read_to_string(&token_file)?;
        Ok(token.trim().to_string())
    } else {
        // Generate secure random token
        let token = format!("pc_{}", Uuid::new_v4().to_string().replace("-", ""));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::write(&token_file, &token)?;
            fs::set_permissions(&token_file, fs::Permissions::from_mode(0o600))?;
        }
        #[cfg(not(unix))]
        {
            fs::write(&token_file, &token)?;
        }

        Ok(token)
    }
}

/// Strip the port and any IPv6 brackets from a `Host`/authority value.
fn hostname_of(authority: &str) -> &str {
    if let Some(rest) = authority.strip_prefix('[') {
        // IPv6 literal: [::1]:port -> ::1
        rest.split(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    }
}

/// EXACT loopback host match (defeats DNS-rebinding names like `127.0.0.1.evil.com`).
fn host_is_loopback(host: &str) -> bool {
    if host.is_empty() {
        // Some loopback clients omit Host; the bearer token still gates the request.
        return true;
    }
    let h = hostname_of(host).to_ascii_lowercase();
    h == "localhost" || h == "127.0.0.1" || h == "::1"
}

/// EXACT loopback origin match (a rebinding origin like `http://localhost.evil.com`
/// has hostname `localhost.evil.com`, which is rejected).
fn origin_is_allowed(origin: &str) -> bool {
    let o = origin.to_ascii_lowercase();
    if o == "null" || o == "tauri://localhost" {
        return true;
    }
    let rest = match o
        .strip_prefix("http://")
        .or_else(|| o.strip_prefix("https://"))
    {
        Some(r) => r,
        None => return false,
    };
    let host = hostname_of(rest.split('/').next().unwrap_or(""));
    host == "localhost" || host == "127.0.0.1" || host == "::1"
}

pub async fn auth_middleware(
    token: String,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    // 1. Host header validation (exact loopback match — anti-DNS-rebinding).
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if !host_is_loopback(host) {
        return Err(StatusCode::BAD_REQUEST);
    }

    // 2. Origin validation (if present).
    if let Some(origin_val) = req.headers().get(header::ORIGIN) {
        if let Ok(origin) = origin_val.to_str() {
            if !origin_is_allowed(origin) {
                return Err(StatusCode::FORBIDDEN);
            }
        }
    }

    // 3. Authorization validation
    // Check Authorization header first
    let mut authed = false;
    if let Some(auth_header) = req.headers().get(header::AUTHORIZATION) {
        if let Ok(auth_str) = auth_header.to_str() {
            if let Some(bearer_token) = auth_str.strip_prefix("Bearer ") {
                if constant_time_eq(bearer_token.trim().as_bytes(), token.as_bytes()) {
                    authed = true;
                }
            }
        }
    }

    // Fallback: check query parameter
    if !authed {
        let uri = req.uri();
        if let Some(query) = uri.query() {
            let params = url::form_urlencoded::parse(query.as_bytes());
            for (key, val) in params {
                if (key == "token" || key == "access_token")
                    && constant_time_eq(val.as_bytes(), token.as_bytes())
                {
                    authed = true;
                    break;
                }
            }
        }
    }

    if !authed {
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(next.run(req).await)
}

/// Best-effort constant-time byte comparison for the auth token: it does not
/// short-circuit on the first differing byte, so it doesn't leak the token via
/// response-timing. The length check is not secret (the token is a fixed-length
/// UUID). Avoids a crypto dependency for what is a loopback-only, defence-in-depth
/// hardening over `==`.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches_value_equality() {
        assert!(constant_time_eq(b"hunter2", b"hunter2"));
        assert!(constant_time_eq(b"", b""));
        assert!(!constant_time_eq(b"hunter2", b"hunter3"));
        assert!(!constant_time_eq(b"short", b"longer-token"));
        assert!(!constant_time_eq(b"token", b""));
    }

    #[test]
    fn loopback_host_is_exact_match() {
        // Allowed loopback authorities.
        assert!(host_is_loopback("localhost"));
        assert!(host_is_loopback("127.0.0.1"));
        assert!(host_is_loopback("127.0.0.1:48123"));
        assert!(host_is_loopback("[::1]:48123"));
        assert!(host_is_loopback("LocalHost:80"));
        assert!(host_is_loopback("")); // omitted Host — the bearer token still gates.

        // DNS-rebinding names MUST be rejected (the old `starts_with` let these in).
        assert!(!host_is_loopback("127.0.0.1.evil.com"));
        assert!(!host_is_loopback("127.0.0.1.evil.com:80"));
        assert!(!host_is_loopback("localhost.evil.com"));
        assert!(!host_is_loopback("evil.com"));
    }

    #[test]
    fn origin_is_exact_match() {
        assert!(origin_is_allowed("null"));
        assert!(origin_is_allowed("tauri://localhost"));
        assert!(origin_is_allowed("http://localhost:5173"));
        assert!(origin_is_allowed("http://127.0.0.1:48123"));
        assert!(origin_is_allowed("https://localhost"));

        // Rebinding / spoofed origins MUST be rejected.
        assert!(!origin_is_allowed("http://localhost.evil.com"));
        assert!(!origin_is_allowed("http://127.0.0.1.evil.com"));
        assert!(!origin_is_allowed("https://evil.com"));
        assert!(!origin_is_allowed("http://evil.com/127.0.0.1"));
    }
}
