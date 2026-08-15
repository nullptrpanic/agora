use axum::Router;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

const INDEX: &str = include_str!("../../web/index.html");
const APP_CSS: &str = include_str!("../../web/app.css");
const APP_JS: &str = include_str!("../../web/app.js");
const TIMELINE_FOLLOW_JS: &str = include_str!("../../web/timeline-follow.js");
const TRACE_BATCH_JS: &str = include_str!("../../web/trace-batch.js");
const XTERM_JS: &str = include_str!("../../third-party/xterm/xterm.js");
const XTERM_CSS: &str = include_str!("../../third-party/xterm/xterm.css");
const FIT_JS: &str = include_str!("../../third-party/xterm-addon-fit/addon-fit.js");

const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self' ws://127.0.0.1:*; img-src 'self' data:; font-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";

pub(super) fn routes<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route(
            "/",
            get(|| async { asset(INDEX, "text/html; charset=utf-8") }),
        )
        .route(
            "/app.css",
            get(|| async { asset(APP_CSS, "text/css; charset=utf-8") }),
        )
        .route(
            "/app.js",
            get(|| async { asset(APP_JS, "text/javascript; charset=utf-8") }),
        )
        .route(
            "/timeline-follow.js",
            get(|| async { asset(TIMELINE_FOLLOW_JS, "text/javascript; charset=utf-8") }),
        )
        .route(
            "/trace-batch.js",
            get(|| async { asset(TRACE_BATCH_JS, "text/javascript; charset=utf-8") }),
        )
        .route(
            "/vendor/xterm.js",
            get(|| async { asset(XTERM_JS, "text/javascript; charset=utf-8") }),
        )
        .route(
            "/vendor/xterm.css",
            get(|| async { asset(XTERM_CSS, "text/css; charset=utf-8") }),
        )
        .route(
            "/vendor/addon-fit.js",
            get(|| async { asset(FIT_JS, "text/javascript; charset=utf-8") }),
        )
}

fn asset(body: &'static str, content_type: &'static str) -> Response {
    let mut response = body.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, max-age=0"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::HeaderName::from_static("cross-origin-opener-policy"),
        HeaderValue::from_static("same-origin"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::routes;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    #[tokio::test]
    async fn serves_every_embedded_asset_with_security_headers_and_no_cors() {
        let cases = [
            ("/", "text/html"),
            ("/app.css", "text/css"),
            ("/app.js", "text/javascript"),
            ("/timeline-follow.js", "text/javascript"),
            ("/trace-batch.js", "text/javascript"),
            ("/vendor/xterm.js", "text/javascript"),
            ("/vendor/xterm.css", "text/css"),
            ("/vendor/addon-fit.js", "text/javascript"),
        ];

        for (path, content_type) in cases {
            let response = routes::<()>()
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert!(
                response.headers()[header::CONTENT_TYPE]
                    .to_str()
                    .unwrap()
                    .starts_with(content_type),
                "{path}"
            );
            assert_eq!(
                response.headers()[header::CACHE_CONTROL],
                "no-store, max-age=0"
            );
            let policy = response.headers()[header::CONTENT_SECURITY_POLICY]
                .to_str()
                .unwrap();
            assert!(policy.contains("default-src 'none'"));
            assert!(policy.contains("connect-src 'self' ws://127.0.0.1:*"));
            assert!(
                !response
                    .headers()
                    .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            );
            assert!(
                !to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[test]
    fn application_assets_are_local_only_and_vendor_licenses_are_retained() {
        let application = concat!(
            include_str!("../../web/index.html"),
            include_str!("../../web/timeline-follow.js"),
            include_str!("../../web/trace-batch.js"),
            include_str!("../../web/app.js")
        );
        assert!(!application.contains("http://"));
        assert!(!application.contains("https://"));
        assert!(!application.to_ascii_lowercase().contains("cdn"));
        assert!(!application.contains("localStorage"));
        assert!(!application.contains("indexedDB"));
        assert!(
            include_str!("../../third-party/xterm/LICENSE")
                .contains("Permission is hereby granted")
        );
        assert!(
            include_str!("../../third-party/xterm-addon-fit/LICENSE")
                .contains("Permission is hereby granted")
        );
    }

    #[test]
    fn frontend_helpers_load_before_the_application_and_are_wired() {
        let index = include_str!("../../web/index.html");
        let follow_helper = r#"<script defer src="/timeline-follow.js"></script>"#;
        let batch_helper = r#"<script defer src="/trace-batch.js"></script>"#;
        let application = r#"<script defer src="/app.js"></script>"#;
        assert!(index.find(follow_helper).unwrap() < index.find(batch_helper).unwrap());
        assert!(index.find(batch_helper).unwrap() < index.find(application).unwrap());

        let app = include_str!("../../web/app.js");
        assert!(app.contains("timelineFollowing: true"));
        assert!(app.contains("timelineFollow.isAtBottom(elements.timeline)"));
        assert!(app.contains("timelineFollow.restoreAfterRender("));
    }
}
