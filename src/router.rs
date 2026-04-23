use axum::{
    Router,
    extract::DefaultBodyLimit,
    http::Method,
    middleware::from_extractor_with_state,
    routing::{get, post},
};
use sqlx::SqlitePool;
use tower_http::{compression::CompressionLayer, cors::CorsLayer};

use crate::{
    api::*,
    middleware::{RequireAdminAuth, RequireFlexibleAuth},
    services::{account_pool::AccountPoolHandle, user_limiter::UserLimiterMap},
    state::{AdminEvent, AppState, AuthState},
    stealth,
};

pub struct RouterBuilder {
    state: AppState,
    inner: Router,
}

impl RouterBuilder {
    pub async fn new(db_pool: SqlitePool) -> Self {
        let stealth_profile = stealth::init_stealth_profile(&db_pool).await;
        let (event_tx, _) = tokio::sync::broadcast::channel::<AdminEvent>(64);
        let pool_handle = AccountPoolHandle::start(db_pool.clone(), event_tx.clone())
            .await
            .expect("Failed to start AccountPoolActor");
        let claude_providers = crate::providers::claude::build_providers(
            pool_handle.clone(),
            db_pool.clone(),
            stealth_profile.clone(),
            event_tx.clone(),
        );
        let session_secret = crate::db::load_session_secret(&db_pool)
            .await
            .expect("Failed to load session secret");
        let auth = AuthState {
            db: db_pool.clone(),
            session_secret,
        };
        let state = AppState {
            db: db_pool.clone(),
            account_pool: pool_handle,
            code_provider: claude_providers.code(),
            auth,
            user_limiter: UserLimiterMap::new(),
            stealth_profile,
            event_tx,
        };
        RouterBuilder {
            state,
            inner: Router::new(),
        }
    }

    pub fn with_default_setup(self) -> Self {
        self.route_public_endpoints()
            .route_claude_code_endpoints()
            .route_admin_endpoints()
            .setup_static_serving()
            .with_tower_trace()
            .with_cors()
    }

    fn route_public_endpoints(mut self) -> Self {
        let router = Router::new()
            .route("/health", get(crate::api::health::health))
            .route("/v1/models", get(crate::api::models::list))
            .route("/v1/models/{model_id}", get(crate::api::models::get))
            .with_state(self.state.clone());
        self.inner = self.inner.merge(router);
        self
    }

    fn route_claude_code_endpoints(mut self) -> Self {
        let router = Router::new()
            .route("/v1/messages", post(api_claude_code))
            .route(
                "/v1/messages/count_tokens",
                post(api_claude_code_count_tokens),
            )
            .layer(CompressionLayer::new())
            .route_layer(from_extractor_with_state::<RequireFlexibleAuth, _>(
                self.state.clone(),
            ))
            .with_state(self.state.clone());
        self.inner = self.inner.merge(router);
        self
    }

    fn route_admin_endpoints(mut self) -> Self {
        let admin_router =
            crate::api::admin::admin_router()
                .route_layer(from_extractor_with_state::<RequireAdminAuth, _>(
                    self.state.clone(),
                ));

        let router = Router::new()
            .nest("/api/admin", admin_router)
            .route("/api/version", get(api_version))
            .route("/auth/login", post(crate::api::auth::login))
            .route(
                "/auth/logout",
                post(crate::api::auth::logout)
                    .route_layer(from_extractor_with_state::<RequireAdminAuth, _>(
                        self.state.clone(),
                    )),
            )
            .with_state(self.state.clone());
        self.inner = self.inner.merge(router);
        self
    }

    fn setup_static_serving(mut self) -> Self {
        use axum::http::{StatusCode, Uri, header};
        use axum::response::{IntoResponse, Response};

        // SPA fallback: serve index.html for client-side routes, 404 for API paths
        #[cfg(feature = "embed-resource")]
        {
            use include_dir::{Dir, include_dir};
            const INCLUDE_STATIC: Dir = include_dir!("$CARGO_MANIFEST_DIR/static");

            async fn embed_handler(uri: Uri) -> Response {
                let path = uri.path().trim_start_matches('/');
                // Serve static file if it exists
                if let Some(file) = INCLUDE_STATIC.get_file(path) {
                    let ct = if path.ends_with(".js") {
                        "application/javascript"
                    } else if path.ends_with(".css") {
                        "text/css"
                    } else if path.ends_with(".html") {
                        "text/html; charset=utf-8"
                    } else if path.ends_with(".svg") {
                        "image/svg+xml"
                    } else if path.ends_with(".png") {
                        "image/png"
                    } else if path.ends_with(".ico") {
                        "image/x-icon"
                    } else if path.ends_with(".json") {
                        "application/json"
                    } else {
                        "application/octet-stream"
                    };
                    // Hashed assets get long-lived cache; HTML gets no-cache for update freshness
                    let cc = if path.starts_with("assets/") {
                        "public, max-age=31536000, immutable"
                    } else if path.ends_with(".html") {
                        "no-cache"
                    } else {
                        "public, max-age=3600"
                    };
                    return Response::builder()
                        .header(header::CONTENT_TYPE, ct)
                        .header(header::CACHE_CONTROL, cc)
                        .body(axum::body::Body::from(file.contents()))
                        .unwrap();
                }
                // API paths → 404
                if path.starts_with("api/") || path.starts_with("auth/") || path.starts_with("v1/")
                {
                    return StatusCode::NOT_FOUND.into_response();
                }
                // SPA fallback
                match INCLUDE_STATIC.get_file("index.html") {
                    Some(file) => Response::builder()
                        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                        .header(header::CACHE_CONTROL, "no-cache")
                        .body(axum::body::Body::from(file.contents()))
                        .unwrap(),
                    None => StatusCode::NOT_FOUND.into_response(),
                }
            }

            self.inner = self.inner.fallback(embed_handler);
        }
        #[cfg(feature = "external-resource")]
        {
            use const_format::formatc;
            const STATIC_DIR: &str = formatc!("{}/static", env!("CARGO_MANIFEST_DIR"));
            const INDEX_HTML: &str = formatc!("{}/static/index.html", env!("CARGO_MANIFEST_DIR"));

            async fn external_handler(uri: Uri) -> Response {
                let path = uri.path().trim_start_matches('/');
                // Try serving static file
                let file_path = std::path::Path::new(STATIC_DIR).join(path);
                if file_path.is_file()
                    && let Ok(bytes) = tokio::fs::read(&file_path).await
                {
                    let ct = if path.ends_with(".js") {
                        "application/javascript"
                    } else if path.ends_with(".css") {
                        "text/css"
                    } else if path.ends_with(".html") {
                        "text/html; charset=utf-8"
                    } else if path.ends_with(".svg") {
                        "image/svg+xml"
                    } else if path.ends_with(".png") {
                        "image/png"
                    } else if path.ends_with(".ico") {
                        "image/x-icon"
                    } else if path.ends_with(".json") {
                        "application/json"
                    } else {
                        "application/octet-stream"
                    };
                    let cc = if path.starts_with("assets/") {
                        "public, max-age=31536000, immutable"
                    } else if path.ends_with(".html") {
                        "no-cache"
                    } else {
                        "public, max-age=3600"
                    };
                    return Response::builder()
                        .header(header::CONTENT_TYPE, ct)
                        .header(header::CACHE_CONTROL, cc)
                        .body(axum::body::Body::from(bytes))
                        .unwrap();
                }
                // API paths → 404
                if path.starts_with("api/") || path.starts_with("auth/") || path.starts_with("v1/")
                {
                    return StatusCode::NOT_FOUND.into_response();
                }
                // SPA fallback
                match tokio::fs::read(INDEX_HTML).await {
                    Ok(bytes) => Response::builder()
                        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                        .header(header::CACHE_CONTROL, "no-cache")
                        .body(axum::body::Body::from(bytes))
                        .unwrap(),
                    Err(_) => StatusCode::NOT_FOUND.into_response(),
                }
            }

            self.inner = self.inner.fallback(external_handler);
        }
        self
    }

    fn with_cors(mut self) -> Self {
        use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
        use http::header::HeaderName;

        let cors = CorsLayer::new()
            .allow_origin(tower_http::cors::Any)
            .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
            .allow_headers([
                AUTHORIZATION,
                CONTENT_TYPE,
                HeaderName::from_static("x-api-key"),
            ]);

        self.inner = self.inner.layer(cors);
        self
    }

    fn with_tower_trace(mut self) -> Self {
        use tower_http::trace::TraceLayer;

        let layer = TraceLayer::new_for_http();

        self.inner = self.inner.layer(layer);
        self
    }

    pub fn build(self) -> Router {
        self.inner.layer(DefaultBodyLimit::max(32 * 1024 * 1024))
    }

    /// Access the underlying `AppState` — primarily for integration tests that
    /// need to drive the account pool or billing paths directly.
    pub fn state(&self) -> &AppState {
        &self.state
    }
}
