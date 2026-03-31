pub mod accounts;
pub mod common;
pub mod keys;
pub mod overview;
pub mod policies;
pub mod requests;
pub mod settings;
pub mod users;

use axum::{
    Router,
    routing::{delete, get, put},
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
        // Keys
        .route("/keys", get(keys::list).post(keys::create))
        .route("/keys/{id}", delete(keys::remove))
        // Accounts
        .route("/accounts", get(accounts::list).post(accounts::create))
        .route(
            "/accounts/{id}",
            put(accounts::update).delete(accounts::remove),
        )
        // Settings
        .route("/settings", get(settings::get_all).post(settings::update))
        // Request logs
        .route("/requests", get(requests::list))
        // Overview
        .route("/overview", get(overview::overview))
}
