use axum::{
    Router,
    extract::DefaultBodyLimit,
    http::Method,
    middleware::from_extractor_with_state,
    routing::{delete, get, post},
};
use sqlx::SqlitePool;
use tower_http::{compression::CompressionLayer, cors::CorsLayer};

use crate::{
    api::*,
    config::CLEWDR_CONFIG,
    middleware::{RequireAdminAuth, RequireFlexibleAuth},
    services::{cookie_actor::CookieActorHandle, user_limiter::UserLimiterMap},
    state::{AppState, AuthState},
    stealth,
};

pub struct RouterBuilder {
    state: AppState,
    inner: Router,
}

impl RouterBuilder {
    pub async fn new(db_pool: SqlitePool) -> Self {
        let cookie_handle = CookieActorHandle::start()
            .await
            .expect("Failed to start CookieActor");
        let stealth_profile = stealth::init_stealth_profile(&db_pool).await;
        let claude_providers = crate::providers::claude::build_providers(cookie_handle.clone(), db_pool.clone(), stealth_profile.clone());
        let config = CLEWDR_CONFIG.load();
        let pw = config.get_password();
        let admin_pw = config.get_admin_password();
        let auth = AuthState {
            db: db_pool.clone(),
            legacy_user_password: if pw.is_empty() { None } else { Some(pw.to_owned()) },
            legacy_admin_password: if admin_pw.is_empty() { None } else { Some(admin_pw.to_owned()) },
        };
        let state = AppState {
            db: db_pool.clone(),
            cookie_actor: cookie_handle,
            code_provider: claude_providers.code(),
            auth,
            user_limiter: UserLimiterMap::new(),
            stealth_profile,
        };
        RouterBuilder {
            state,
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
            .layer(CompressionLayer::new())
            .route_layer(from_extractor_with_state::<RequireFlexibleAuth, _>(self.state.clone()))
            .with_state(self.state.clone());
        self.inner = self.inner.merge(router);
        self
    }

    fn route_admin_endpoints(mut self) -> Self {
        let admin_router = Router::new()
            .route("/cookies", get(api_get_cookies))
            .route(
                "/cookie",
                delete(api_delete_cookie)
                    .post(api_post_cookie)
                    .put(api_put_cookie),
            )
            .route("/auth", get(api_auth))
            .route("/config", get(api_get_config).post(api_post_config))
            .route_layer(from_extractor_with_state::<RequireAdminAuth, _>(self.state.clone()));
        let router = Router::new()
            .nest("/api", admin_router)
            .route("/api/version", get(api_version))
            .with_state(self.state.clone());
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
