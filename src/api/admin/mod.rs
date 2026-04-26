pub mod accounts;
pub mod common;
pub mod events;
pub mod keys;
mod me;
pub mod models;
pub mod ops;
pub mod overview;
pub mod policies;
pub mod proxies;
pub mod requests;
pub mod settings;
pub mod users;

use axum::{
    Router,
    routing::{delete, get, post, put},
};

use crate::state::AppState;

pub fn admin_router() -> Router<AppState> {
    Router::new()
        // Policies
        .route("/policies", get(policies::list).post(policies::create))
        .route(
            "/policies/{id}",
            put(policies::update).delete(policies::remove),
        )
        // Users
        .route("/users", get(users::list).post(users::create))
        .route("/users/{id}", put(users::update).delete(users::remove))
        .route("/users/{id}/usage/reset", post(users::reset_usage))
        // Keys
        .route("/keys", get(keys::list).post(keys::create))
        .route("/keys/{id}", delete(keys::remove))
        .route("/keys/{id}/bindings", put(keys::update_bindings))
        // Accounts
        .route("/accounts", get(accounts::list).post(accounts::create))
        .route("/accounts/probe", post(accounts::probe_all))
        .route("/accounts/oauth/start", post(accounts::start_oauth))
        .route(
            "/accounts/{id}",
            put(accounts::update).delete(accounts::remove),
        )
        .route("/accounts/{id}/test", post(accounts::test_account))
        // Proxies
        .route("/proxies", get(proxies::list).post(proxies::create))
        .route(
            "/proxies/{id}",
            put(proxies::update).delete(proxies::remove),
        )
        .route("/proxies/{id}/test", post(proxies::test))
        // Models
        .route("/models", get(models::list).post(models::create))
        .route("/models/reset-defaults", post(models::reset_defaults))
        .route(
            "/models/{model_id}",
            put(models::update).delete(models::remove),
        )
        // Settings
        .route("/settings", get(settings::get_all).post(settings::update))
        // CLI versions (npm)
        .route("/cli-versions", get(settings::cli_versions))
        // Ops analytics
        .route("/ops/usage", get(ops::usage))
        // Request logs
        .route("/requests", get(requests::list))
        .route(
            "/requests/{id}/response_body",
            get(requests::get_response_body),
        )
        // SSE events
        .route("/events", get(events::events))
        // Overview
        .route("/overview", get(overview::overview))
        // Change own password
        .route("/me/password", put(me::change_password))
}
