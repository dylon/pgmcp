use axum::http::HeaderMap;
use axum::http::header::{AUTHORIZATION, ORIGIN};

pub fn origin_allowed(headers: &HeaderMap, allowed: &[String]) -> bool {
    let Some(origin) = headers.get(ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    allowed.iter().any(|candidate| candidate == origin)
}

pub fn token_authorized(
    headers: &HeaderMap,
    query_token: Option<&str>,
    expected: Option<&str>,
) -> bool {
    let Some(expected) = expected.filter(|s| !s.is_empty()) else {
        return true;
    };

    let query_matches = query_token
        .filter(|s| !s.is_empty())
        .map(|token| constant_time_eq(token.as_bytes(), expected.as_bytes()))
        .unwrap_or(false);
    let bearer_matches = bearer_token(headers)
        .map(|token| constant_time_eq(token.as_bytes(), expected.as_bytes()))
        .unwrap_or(false);
    query_matches | bearer_matches
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    value.strip_prefix("Bearer ")
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let max_len = left.len().max(right.len());
    let mut diff = left.len() ^ right.len();
    for idx in 0..max_len {
        let l = left.get(idx).copied().unwrap_or(0);
        let r = right.get(idx).copied().unwrap_or(0);
        diff |= (l ^ r) as usize;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(*name, HeaderValue::from_static(value));
        }
        headers
    }

    #[test]
    fn origin_check_allows_non_browser_clients_without_origin() {
        assert!(origin_allowed(
            &HeaderMap::new(),
            &["http://127.0.0.1:43100".to_string()]
        ));
    }

    #[test]
    fn origin_check_requires_exact_allowed_origin_when_origin_is_present() {
        let allowed = vec!["http://127.0.0.1:43100".to_string()];

        assert!(origin_allowed(
            &headers(&[("origin", "http://127.0.0.1:43100")]),
            &allowed
        ));
        assert!(!origin_allowed(
            &headers(&[("origin", "http://evil.example")]),
            &allowed
        ));
    }

    #[test]
    fn missing_expected_token_disables_auth_requirement() {
        assert!(token_authorized(&HeaderMap::new(), None, None));
        assert!(token_authorized(&HeaderMap::new(), None, Some("")));
    }

    #[test]
    fn token_auth_accepts_query_or_bearer_credentials() {
        assert!(token_authorized(
            &HeaderMap::new(),
            Some("secret"),
            Some("secret")
        ));
        assert!(token_authorized(
            &headers(&[("authorization", "Bearer secret")]),
            None,
            Some("secret")
        ));
        assert!(token_authorized(
            &headers(&[("authorization", "Bearer secret")]),
            Some("wrong"),
            Some("secret")
        ));
    }

    #[test]
    fn token_auth_rejects_missing_blank_or_wrong_credentials() {
        assert!(!token_authorized(&HeaderMap::new(), None, Some("secret")));
        assert!(!token_authorized(
            &headers(&[("authorization", "Bearer wrong")]),
            Some(""),
            Some("secret")
        ));
        assert!(!token_authorized(
            &headers(&[("authorization", "Basic secret")]),
            Some("wrong"),
            Some("secret")
        ));
    }

    #[test]
    fn constant_time_comparison_matches_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }
}
