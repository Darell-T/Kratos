use axum::extract::Request;
use axum::http::Uri;
use axum::middleware::Next;
use axum::response::Response;

/// TypeScript HTTP routes ignore empty segments after checking the `/api/` prefix. Its socket
/// upgrade handler requires exactly `/api/events`, so that endpoint keeps its original spelling.
pub(super) async fn normalize_http_api_path(mut request: Request, next: Next) -> Response {
    let path = request.uri().path();
    if path.starts_with("/api/") && (path.ends_with('/') || path.contains("//")) {
        let normalized = format!(
            "/{}",
            path.split('/')
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("/")
        );
        if normalized != "/api/events" {
            let path_and_query = match request.uri().query() {
                Some(query) => format!("{normalized}?{query}"),
                None => normalized,
            };
            let mut parts = request.uri().clone().into_parts();
            parts.path_and_query = Some(
                path_and_query
                    .parse()
                    .expect("removing path separators preserves a valid URI"),
            );
            *request.uri_mut() =
                Uri::from_parts(parts).expect("the URI's scheme and authority are unchanged");
        }
    }
    next.run(request).await
}
