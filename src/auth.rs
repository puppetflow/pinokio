use axum::http::HeaderMap;
use subtle::ConstantTimeEq;

/// Checks the client token against the configured one.
///
/// The token is accepted either as a `?token=` query parameter or as an
/// `Authorization: Bearer` header. When no token is configured,
/// authentication is disabled and every request is accepted.
///
/// Comparison is constant-time on the token bytes. Length is still
/// observable, which is acceptable for this threat model.
pub fn is_authorized(
    expected: Option<&str>,
    query_token: Option<&str>,
    headers: &HeaderMap,
) -> bool {
    let Some(expected) = expected else {
        return true;
    };

    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);

    let provided = match query_token.or(bearer) {
        Some(t) => t,
        None => return false,
    };

    provided.as_bytes().ct_eq(expected.as_bytes()).into()
}
