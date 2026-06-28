//! Internal articles REST (SOUL §11) — the worked how-to recipes that sit above the
//! node-type catalog, for both the visual editor and tool-using agents.
//!
//! Static, global documentation (identical for every workspace, no workspace data or
//! secrets), gated `automation:read` like the node-type catalog it complements — the
//! articles teach how to *author* automations.
//!
//! Routes:
//! - `GET /articles`               the full article corpus (unranked)
//! - `GET /articles/search?q=…&limit=N`  semantic article search
//! - `GET /articles/{id}`          one article by id (`404` if absent)

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use catalerum_automation::Article;
use catalerum_core::capability::Action;

use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Default / max article search results.
const ARTICLE_SEARCH_DEFAULT_LIMIT: usize = 5;
const ARTICLE_SEARCH_MAX_LIMIT: usize = 10;

/// Mount the article routes.
pub fn router() -> Router<AppState> {
    Router::new()
        // The static `search` segment is registered alongside `{id}`; axum prefers a
        // literal path over a `{id}` capture, so `/articles/search` resolves before
        // `/articles/{id}` (an article can't be named "search").
        .route("/articles", get(list_articles))
        .route("/articles/search", get(search_articles))
        .route("/articles/{id}", get(get_article))
}

/// One ranked article result: the full [`Article`] plus its relevance score.
#[derive(Debug, Serialize)]
pub struct ArticleHitBody {
    #[serde(flatten)]
    pub article: Article,
    /// Cosine similarity to the query (higher is closer); `0.0` for the unranked list.
    pub score: f32,
}

/// `GET /articles` — the full article corpus (every how-to recipe with its body,
/// tags, and cross-linked node types). Static, global documentation; gated
/// `automation:read`. Returns the **same shape** as the search endpoint with `score`
/// fixed at `0.0` (this listing is unranked), so a client decodes both identically.
async fn list_articles(auth: Auth) -> ApiResult<Json<Vec<ArticleHitBody>>> {
    auth.require(Action::Read, "automation")?;
    let out = catalerum_automation::articles()
        .iter()
        .cloned()
        .map(|article| ArticleHitBody {
            article,
            score: 0.0,
        })
        .collect();
    Ok(Json(out))
}

/// Query for `GET /articles/search` — `?q=…&limit=N`.
#[derive(Debug, Deserialize)]
pub struct ArticleSearchQuery {
    /// Natural-language description of what the author wants to build or learn.
    #[serde(default)]
    pub q: String,
    /// Max results (clamped to `[1, ARTICLE_SEARCH_MAX_LIMIT]`).
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `GET /articles/search?q=…&limit=N` — semantically rank the article corpus against
/// `q` (SOUL §11). Empty `q` → `400`. Gated `automation:read`.
async fn search_articles(
    State(state): State<AppState>,
    auth: Auth,
    Query(q): Query<ArticleSearchQuery>,
) -> ApiResult<Json<Vec<ArticleHitBody>>> {
    auth.require(Action::Read, "automation")?;
    let query = q.q.trim();
    if query.is_empty() {
        return Err(ApiError::bad_request("search query `q` must not be empty"));
    }
    let limit = q
        .limit
        .unwrap_or(ARTICLE_SEARCH_DEFAULT_LIMIT)
        .clamp(1, ARTICLE_SEARCH_MAX_LIMIT);
    let hits = state.article_index().search(query, limit).await?;
    let out = hits
        .into_iter()
        .map(|h| ArticleHitBody {
            article: h.article,
            score: h.score,
        })
        .collect();
    Ok(Json(out))
}

/// `GET /articles/{id}` — one article by its slug (SOUL §11). `404` if absent. Gated
/// `automation:read`.
async fn get_article(auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Article>> {
    auth.require(Action::Read, "automation")?;
    catalerum_automation::articles::get(&id)
        .cloned()
        .map(Json)
        .ok_or(ApiError::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn router_builds_with_overlapping_search_and_id_routes() {
        // `/articles/search` is registered alongside the `/articles/{id}` capture.
        // Route insertion happens here, so a matchit overlap conflict would panic on
        // build — assert it doesn't (the literal `search` wins over the capture).
        let _: Router<AppState> = router();
    }

    #[test]
    fn article_search_limit_clamps() {
        let clamp = |n: Option<usize>| {
            n.unwrap_or(ARTICLE_SEARCH_DEFAULT_LIMIT)
                .clamp(1, ARTICLE_SEARCH_MAX_LIMIT)
        };
        assert_eq!(clamp(None), ARTICLE_SEARCH_DEFAULT_LIMIT);
        assert_eq!(clamp(Some(0)), 1, "zero clamps up to 1");
        assert_eq!(
            clamp(Some(999)),
            ARTICLE_SEARCH_MAX_LIMIT,
            "over-large is capped"
        );
        assert_eq!(clamp(Some(3)), 3);
    }
}
