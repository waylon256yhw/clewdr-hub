use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
pub struct PaginationParams {
    pub offset: Option<i64>,
    pub limit: Option<i64>,
}

impl PaginationParams {
    pub fn resolve(&self) -> (i64, i64) {
        let offset = self.offset.unwrap_or(0).max(0);
        let limit = self.limit.unwrap_or(50).clamp(1, 100);
        (offset, limit)
    }
}

#[derive(Serialize)]
pub struct Paginated<T: Serialize> {
    pub items: Vec<T>,
    pub total: i64,
    pub offset: i64,
    pub limit: i64,
}
