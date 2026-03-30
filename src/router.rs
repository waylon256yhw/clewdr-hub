use axum::{
    Router,
    extract::DefaultBodyLimit,
    http::Method,
    middleware::from_extractor,
    routing::{delete, get, post},
};
use sqlx::SqlitePool;
use tower::ServiceBuilder;
use tower_http::{compression::CompressionLayer, cors::CorsLayer};

use crate::{
    api::*,
    middleware::{RequireAdminAuth, RequireFlexibleAuth},
    providers::claude::ClaudeProviders,
    services::cookie_actor::CookieActorHandle,
};

/// RouterBuilder for the application
pub struct RouterBuilder {
    claude_providers: ClaudeProviders,
    cookie_actor_handle: CookieActorHandle,
    #[allow(dead_code)]
    db_pool: SqlitePool,
    inner: Router,
}

impl RouterBuilder {
    pub async fn new(db_pool: SqlitePool) -> Self {
        let cookie_handle = CookieActorHandle::start()
            .await
            .expect("Failed to start CookieActor");
        let claude_providers = crate::providers::claude::build_providers(cookie_handle.clone());
        RouterBuilder {
            claude_providers,
            cookie_actor_handle: cookie_handle,
            db_pool,
            inner: Router::new(),
        }
    }

    pub fn with_default_setup(self) -> Self {
        self.route_claude_code_endpoints()
            .route_admin_endpoints()
            .setup_static_serving()
            .with_tower_trace()
            .with_cors()
    }

    fn route_claude_code_endpoints(mut self) -> Self {
        let router = Router::new()
            .route("/v1/messages", post(api_claude_code))
            .route("/v1/messages/count_tokens", post(api_claude_code_count_tokens))
            .layer(
                ServiceBuilder::new()
                    .layer(from_extractor::<RequireFlexibleAuth>())
                    .layer(CompressionLayer::new()),
            )
            .with_state(self.claude_providers.code());
        self.inner = self.inner.merge(router);
        self
    }

    fn route_admin_endpoints(mut self) -> Self {
        let cookie_router = Router::new()
            .route("/cookies", get(api_get_cookies))
            .route(
                "/cookie",
                delete(api_delete_cookie)
                    .post(api_post_cookie)
                    .put(api_put_cookie),
            )
            .with_state(self.cookie_actor_handle.to_owned());
        let admin_router = Router::new()
            .route("/auth", get(api_auth))
            .route("/config", get(api_get_config).post(api_post_config));
        let router = Router::new()
            .nest(
                "/api",
                cookie_router
                    .merge(admin_router)
                    .layer(from_extractor::<RequireAdminAuth>()),
            )
            .route("/api/version", get(api_version));
        self.inner = self.inner.merge(router);
        self
    }

    fn setup_static_serving(mut self) -> Self {
        #[cfg(feature = "embed-resource")]
        {
            use include_dir::{Dir, include_dir};
            const INCLUDE_STATIC: Dir = include_dir!("$CARGO_MANIFEST_DIR/static");
            self.inner = self
                .inner
                .fallback_service(tower_serve_static::ServeDir::new(&INCLUDE_STATIC));
        }
        #[cfg(feature = "external-resource")]
        {
            use const_format::formatc;
            use tower_http::services::ServeDir;
            self.inner = self.inner.fallback_service(ServeDir::new(formatc!(
                "{}/static",
                env!("CARGO_MANIFEST_DIR")
            )));
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
}
