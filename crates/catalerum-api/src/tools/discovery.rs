//! Tool/model discovery: search_tools/list_tools/search_models + search node types/articles.

use super::*;

/// Register the agent-profile-authoring tools (SOUL §19/§25). All five are gated on
/// the admin-only `agent_profile` domain; mirrors `register_automation_tools`.
pub(crate) fn register_agent_profile_tools(registry: &mut ToolRegistry, store: &Store) {
    registry.register(Arc::new(ListAgentProfilesTool {
        store: store.clone(),
    }));
    registry.register(Arc::new(GetAgentProfileTool {
        store: store.clone(),
    }));
    registry.register(Arc::new(CreateAgentProfileTool {
        store: store.clone(),
    }));
    registry.register(Arc::new(UpdateAgentProfileTool {
        store: store.clone(),
    }));
    registry.register(Arc::new(DeleteAgentProfileTool {
        store: store.clone(),
    }));
}

// ===========================================================================
// Tool search (SOUL §6.4/§7)
// ===========================================================================

/// `search_tools` — semantically search the available tools by intent, so the
/// agent can discover the right one instead of being shown every spec. Returns
/// only tools the caller is **allowed** to call (capability-gated, §19). Ungated
/// itself: discovering a tool you may use is harmless, and results are already
/// filtered to your authority.
pub(crate) struct SearchToolsTool {
    pub(crate) index: Arc<ToolIndex>,
    /// A snapshot sharing the runtime overlay `Arc`, so search sees both static
    /// tools and hot-connected MCP tools.
    pub(crate) registry: ToolRegistry,
}

#[async_trait]
impl Tool for SearchToolsTool {
    fn name(&self) -> &str {
        SEARCH_TOOLS_NAME
    }
    fn description(&self) -> &str {
        "Find callable tools — actions you can invoke as tool calls, e.g. web \
         search, fetching a URL, running code in a sandbox, sending a chat \
         message — by describing what you want to do. This searches the tool \
         catalog itself, not your documents or data. Returns matching tools — \
         name, description, argument schema, and a relevance score — only tools \
         you're allowed to call. Every tool this returns becomes callable. Use it \
         to discover capabilities (including ones from connected MCP servers) \
         beyond the always-advertised subset; `list_tools` browses the full \
         catalog instead."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Natural-language description of the capability you need." },
                "limit": {
                    "type": "integer",
                    "description": "Max tools to return (1-25, default 10).",
                    "minimum": 1,
                    "maximum": 25
                }
            },
            "required": ["query"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let query = required_str(&args, "query")?;
        let limit = opt_clamped_u64(&args, "limit", 10, 25) as usize;
        let hits = self
            .index
            .search(&self.registry, ctx, &query, limit)
            .await?;
        // Each hit carries its full argument schema: the result is what makes the
        // tool callable on a deferred-advertising run (SOUL §7) — the agent loop
        // widens the advertised set from it, and the schema persisting in the
        // transcript keeps the tool usable on later turns of the conversation.
        let tools: Vec<Json> = hits
            .iter()
            .map(|h| {
                let parameters = self
                    .registry
                    .get(&h.name)
                    .map(|t| t.parameters_schema())
                    .unwrap_or_else(|| json!({ "type": "object" }));
                json!({
                    "name": h.name,
                    "description": h.description,
                    "parameters": parameters,
                    "score": h.score,
                })
            })
            .collect();
        Ok(json!({ "tools": tools }))
    }
}

/// `list_tools` — browse the whole tool catalog (names + one-line descriptions),
/// or fetch specific tools' full argument schemas by name. The exact-name sibling
/// of `search_tools`: together they are the **discovery subset** a
/// deferred-advertising run (SOUL §7) is seeded with, so the model can reach every
/// tool without every spec being shipped on every request. Ungated and
/// capability-filtered exactly like `search_tools`.
pub(crate) struct ListToolsTool {
    /// A snapshot sharing the runtime overlay `Arc`, so the catalog sees both
    /// static tools and hot-connected MCP tools.
    pub(crate) registry: ToolRegistry,
}

#[async_trait]
impl Tool for ListToolsTool {
    fn name(&self) -> &str {
        LIST_TOOLS_NAME
    }
    fn description(&self) -> &str {
        "List every tool you may call — names and one-line descriptions, optionally \
         narrowed by a `filter` substring. Pass `names: [...]` instead to fetch the \
         full argument schemas of specific tools; every tool returned that way \
         becomes callable. Use `search_tools` to find tools by intent when you don't \
         know the name."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "filter": {
                    "type": "string",
                    "description": "Optional case-insensitive substring to match against tool names and descriptions."
                },
                "names": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional exact tool names to load: returns their full argument schemas and makes them callable. Omit to browse the catalog."
                }
            }
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        // Exact selection: full specs, under the same `"tools"` key `search_tools`
        // uses — the shape the agent loop's deferred advertising widens from
        // (SOUL §7). Unknown / not-callable names are reported, never fabricated.
        if let Some(names) = args.get("names").and_then(Json::as_array) {
            let mut tools = Vec::new();
            let mut unknown = Vec::new();
            for name in names.iter().filter_map(Json::as_str) {
                match self.registry.get(name) {
                    Some(t) if tool_allowed(&self.registry, ctx, name) => tools.push(json!({
                        "name": t.name(),
                        "description": t.description(),
                        "parameters": t.parameters_schema(),
                    })),
                    _ => unknown.push(name.to_string()),
                }
            }
            let mut out = json!({ "tools": tools });
            if !unknown.is_empty() {
                out["unknown"] = json!(unknown);
            }
            return Ok(out);
        }
        // Catalog: names + descriptions only (no schemas — that's what `names` /
        // `search_tools` are for), capability-filtered, sorted for determinism.
        let filter = args
            .get("filter")
            .and_then(Json::as_str)
            .map(str::to_lowercase)
            .filter(|f| !f.trim().is_empty());
        let mut catalog: Vec<(String, String)> = self
            .registry
            .specs(None)
            .into_iter()
            .filter(|s| s.name != SEARCH_TOOLS_NAME && s.name != LIST_TOOLS_NAME)
            .filter(|s| tool_allowed(&self.registry, ctx, &s.name))
            .filter(|s| {
                filter.as_ref().is_none_or(|f| {
                    s.name.to_lowercase().contains(f) || s.description.to_lowercase().contains(f)
                })
            })
            .map(|s| (s.name, s.description))
            .collect();
        catalog.sort();
        let entries: Vec<Json> = catalog
            .into_iter()
            .map(|(name, description)| json!({ "name": name, "description": description }))
            .collect();
        Ok(json!({
            "count": entries.len(),
            "catalog": entries,
            "note": "Call list_tools again with names: [...] (or search_tools) to load a tool's argument schema and make it callable.",
        }))
    }
}

/// The model-catalog discovery tool's registered name. Referenced by the standing
/// delegation guidance ([`crate::guidance::DELEGATE_GUIDANCE`]), so the
/// deferred-advertising seeds that promise the nudge's tools (SOUL §7) name it too.
pub(crate) const SEARCH_MODELS_NAME: &str = "search_models";

/// `search_models` — search the gateway's **model catalog** by name / id, so the
/// agent can resolve the exact model id to pass to `delegate` (a cheaper model for
/// a routine subtask) or to `speech_to_text`/`text_to_speech`. Ungated like the
/// other discovery tools: the catalog is global gateway metadata (no workspace
/// data, no secrets) — the same list `/llm-models` shows any authenticated user.
pub(crate) struct SearchModelsTool {
    pub(crate) client: OpenRouterClient,
}

/// Filter a fetched model `catalog` by a case-insensitive substring `query` over
/// model **id and display name** (`None` browses the whole catalog). Exact-id
/// matches rank first, then id matches, then name-only matches; ties sort by id
/// for determinism. Pure, so the ranking is unit-testable without a gateway.
pub(crate) fn filter_model_catalog(catalog: Vec<ModelInfo>, query: Option<&str>) -> Vec<ModelInfo> {
    let q = query
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .map(str::to_lowercase);
    let mut ranked: Vec<(u8, ModelInfo)> = catalog
        .into_iter()
        .filter_map(|m| {
            let Some(q) = q.as_deref() else {
                return Some((2, m));
            };
            let id = m.id.to_lowercase();
            if id == q {
                Some((0, m))
            } else if id.contains(q) {
                Some((1, m))
            } else if m.name.to_lowercase().contains(q) {
                Some((2, m))
            } else {
                None
            }
        })
        .collect();
    ranked.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.id.cmp(&b.1.id)));
    ranked.into_iter().map(|(_, m)| m).collect()
}

/// Parse the tool's `kind` argument into a [`ModelKind`]. Defaults to
/// [`ModelKind::Chat`] — the delegation use case — and rejects an unknown value
/// (rather than the REST route's silent fall-back to `All`) so the model corrects
/// its call instead of quietly searching the wrong class.
pub(crate) fn parse_model_kind(kind: Option<&str>) -> Result<ModelKind> {
    match kind.map(str::trim) {
        None | Some("" | "chat" | "llm") => Ok(ModelKind::Chat),
        Some("tts") => Ok(ModelKind::Tts),
        Some("stt") => Ok(ModelKind::Stt),
        Some("embedding") => Ok(ModelKind::Embedding),
        Some("all") => Ok(ModelKind::All),
        Some(other) => Err(Error::invalid(format!(
            "unknown model kind `{other}`; use chat, tts, stt, embedding, or all"
        ))),
    }
}

#[async_trait]
impl Tool for SearchModelsTool {
    fn name(&self) -> &str {
        SEARCH_MODELS_NAME
    }
    fn description(&self) -> &str {
        "Search the available LLM models by name or id — use it to find the exact \
         model id to pass to `delegate` (e.g. a cheaper model for a routine subtask) \
         or to speech tools. Returns each match's id (what you pass as `model`) and \
         display name, plus context window and per-token USD pricing when known. \
         `kind` picks the model class (default `chat`; also `tts`, `stt`, \
         `embedding`, `all`); omit `query` to browse the catalog."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Case-insensitive substring matched against model ids and display names (e.g. \"haiku\", \"gpt\"). Omit to browse the whole catalog." },
                "kind": {
                    "type": "string",
                    "enum": ["chat", "tts", "stt", "embedding", "all"],
                    "description": "Model class to search (default `chat` — the class `delegate` needs)."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max models to return (1-25, default 10).",
                    "minimum": 1,
                    "maximum": 25
                }
            }
        })
    }
    async fn invoke(&self, args: Json, _ctx: &ToolContext) -> Result<Json> {
        let kind = parse_model_kind(opt_str_some(&args, "kind").as_deref())?;
        let limit = opt_clamped_u64(&args, "limit", 10, 25) as usize;
        // Fetch the whole kind-filtered catalog and match locally: the gateway's
        // `search` param covers ids only, while the ask here is name-or-id.
        let catalog = self.client.list_models(kind, None).await?;
        let matches = filter_model_catalog(catalog, opt_str_some(&args, "query").as_deref());
        let count = matches.len();
        let models: Vec<Json> = matches
            .into_iter()
            .take(limit)
            .map(|m| {
                let mut j = json!({ "id": m.id, "name": m.name });
                if let Some(cl) = m.context_length {
                    j["context_length"] = json!(cl);
                }
                if let Some(p) = m.prompt_price {
                    j["prompt_price"] = json!(p);
                }
                if let Some(p) = m.completion_price {
                    j["completion_price"] = json!(p);
                }
                j
            })
            .collect();
        let mut out = json!({ "count": count, "models": models });
        // No silent caps: say when matches were dropped so the agent narrows the
        // query instead of assuming it saw everything.
        if count > limit {
            out["note"] = json!(format!(
                "{count} models matched; showing the first {limit} — narrow `query` or raise `limit`."
            ));
        }
        Ok(out)
    }
}

/// Register `search_models` over the gateway `client`. Grouped with the other
/// discovery tools; registered before `register_search_tools` so it lands in that
/// snapshot and is itself tool-searchable.
pub(crate) fn register_search_models(registry: &mut ToolRegistry, client: OpenRouterClient) {
    registry.register(Arc::new(SearchModelsTool { client }));
}

/// Register the discovery tools — `search_tools` + `list_tools` (SOUL §7).
/// Cloning the registry **after** every other tool is registered means they see
/// the whole set (the clone also shares the runtime MCP overlay); both exclude
/// the discovery tools themselves from results. Call this last.
pub(crate) fn register_search_tools(registry: &mut ToolRegistry, index: Arc<ToolIndex>) {
    let snapshot = registry.clone();
    registry.register(Arc::new(SearchToolsTool {
        index,
        registry: snapshot.clone(),
    }));
    registry.register(Arc::new(ListToolsTool { registry: snapshot }));
}

/// The pinned subset a **deferred-advertising** agent run is seeded with
/// (SOUL §7): the discovery tools plus the general-purpose JavaScript sandbox.
/// Keeping `run_javascript` in this shared seed makes exact computation and data
/// shaping available immediately in chat, profiles, agents, and skills instead of
/// requiring a discovery round first. Callers append whatever other tools their
/// standing prompts promise the model (chat: `delegate`, the memory trio,
/// `ask_user`), then hand the list to
/// [`AgentConfig::discovery_tools`](catalerum_llm::AgentConfig).
pub(crate) fn discovery_seed() -> Vec<String> {
    vec![
        SEARCH_TOOLS_NAME.to_string(),
        LIST_TOOLS_NAME.to_string(),
        RUN_JAVASCRIPT_NAME.to_string(),
    ]
}

/// `search_automation_node_types` — semantically search the automation node-type
/// catalog (SOUL §11) so an agent authoring an automation can find the trigger/action/
/// code/condition node **type** it needs by intent, then read its params + example
/// before placing a node of that type. (A *node* is an instance of a node *type* in a
/// specific graph; this searches the types.) Ungated: the catalog is global
/// documentation (no workspace data, no secrets) — exactly like `search_tools`,
/// discovering a node type you might use is harmless.
pub(crate) struct SearchAutomationNodeTypesTool {
    pub(crate) index: Arc<crate::node_index::NodeDocIndex>,
}

#[async_trait]
impl Tool for SearchAutomationNodeTypesTool {
    fn name(&self) -> &str {
        "search_automation_node_types"
    }
    fn description(&self) -> &str {
        "Find automation node types by describing what you want (e.g. \"run every \
         morning\", \"when an email arrives\", \"post to a channel\", \"branch on a \
         condition\"). A node type is a template — a trigger/action/code/condition — \
         that you instantiate as a node in an automation graph. Returns matching node \
         types — id, title, summary, full description, typed params, and a \
         ready-to-paste example node — ranked by relevance. Use this to discover the \
         node types available before authoring an automation with \
         create_automation/test_automation; get_automation_node_type reads one in full."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Natural-language description of the node type you need." },
                "limit": {
                    "type": "integer",
                    "description": "Max node types to return (1-24, default 8).",
                    "minimum": 1,
                    "maximum": 24
                }
            },
            "required": ["query"]
        })
    }
    async fn invoke(&self, args: Json, _ctx: &ToolContext) -> Result<Json> {
        let query = required_str(&args, "query")?;
        let limit = opt_clamped_u64(&args, "limit", 8, 24) as usize;
        let hits = self.index.search(&query, limit).await?;
        let node_types: Vec<Json> = hits
            .iter()
            .map(|h| {
                json!({
                    "id": h.doc.id,
                    "node_kind": h.doc.node_kind,
                    "kind": h.doc.kind,
                    "title": h.doc.title,
                    "summary": h.doc.summary,
                    "description": h.doc.description,
                    "params": h.doc.params,
                    "example": h.doc.example,
                    "score": h.score,
                })
            })
            .collect();
        Ok(json!({ "node_types": node_types }))
    }
}

/// Register `search_automation_node_types` (SOUL §11) over the node-type-catalog
/// `index`. Called from `AppState` (the index needs the embedder, not the store).
pub(crate) fn register_search_automation_node_types(
    registry: &mut ToolRegistry,
    index: Arc<crate::node_index::NodeDocIndex>,
) {
    registry.register(Arc::new(SearchAutomationNodeTypesTool { index }));
}

/// `search_articles` — semantically search the internal **articles** corpus (SOUL §11):
/// curated, worked how-to recipes that walk you through building a whole automation
/// (ingest email, tag email, wire webhooks, index a wiki and serve it over MCP). Where
/// `search_automation_node_types` finds the *one* trigger/action you need, this finds
/// the *recipe* that shows how a task is assembled end-to-end. Ungated: the articles are
/// global documentation (no workspace data, no secrets) — like `search_tools`,
/// discovering a how-to is harmless.
pub(crate) struct SearchArticlesTool {
    pub(crate) index: Arc<crate::article_index::ArticleIndex>,
}

#[async_trait]
impl Tool for SearchArticlesTool {
    fn name(&self) -> &str {
        "search_articles"
    }
    fn description(&self) -> &str {
        "Find internal how-to articles by describing what you want to build (e.g. \
         \"ingest my email\", \"tag incoming mail with labels\", \"trigger from a \
         webhook\", \"index a github wiki and expose it over MCP\"). Each article is a \
         worked, end-to-end automation recipe — the goal, a paste-ready graph, the node \
         params that matter, and the gotchas. Returns matching articles — id, title, \
         summary, tags, the node types they wire together, and the full Markdown body — \
         ranked by relevance. Use this to learn how a task is assembled before authoring \
         it; search_automation_node_types finds an individual node type instead."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Natural-language description of what you want to build or learn." },
                "limit": {
                    "type": "integer",
                    "description": "Max articles to return (1-10, default 5).",
                    "minimum": 1,
                    "maximum": 10
                }
            },
            "required": ["query"]
        })
    }
    async fn invoke(&self, args: Json, _ctx: &ToolContext) -> Result<Json> {
        let query = required_str(&args, "query")?;
        let limit = opt_clamped_u64(&args, "limit", 5, 10) as usize;
        let hits = self.index.search(&query, limit).await?;
        let articles: Vec<Json> = hits
            .iter()
            .map(|h| {
                json!({
                    "id": h.article.id,
                    "title": h.article.title,
                    "summary": h.article.summary,
                    "category": h.article.category,
                    "tags": h.article.tags,
                    "related_nodes": h.article.related_nodes,
                    "body_md": h.article.body_md,
                    "score": h.score,
                })
            })
            .collect();
        Ok(json!({ "articles": articles }))
    }
}

/// Register `search_articles` (SOUL §11) over the internal-articles `index`. Called
/// from `AppState` (the index needs the embedder, not the store).
pub(crate) fn register_search_articles(
    registry: &mut ToolRegistry,
    index: Arc<crate::article_index::ArticleIndex>,
) {
    registry.register(Arc::new(SearchArticlesTool { index }));
}

#[cfg(test)]
mod discovery_tests {
    use super::*;

    #[test]
    fn deferred_advertising_seed_includes_javascript_by_default() {
        assert_eq!(
            discovery_seed(),
            ["search_tools", "list_tools", "run_javascript"]
        );
    }

    /// A no-op tool with a chosen name/description and optional capability gate.
    struct StubTool {
        name: &'static str,
        description: &'static str,
        cap: Option<Capability>,
    }

    #[async_trait]
    impl Tool for StubTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            self.description
        }
        fn required_capability(&self) -> Option<Capability> {
            self.cap.clone()
        }
        fn parameters_schema(&self) -> Json {
            json!({ "type": "object", "properties": { "x": { "type": "string" } } })
        }
        async fn invoke(&self, _args: Json, _ctx: &ToolContext) -> Result<Json> {
            Ok(json!({}))
        }
    }

    fn list_tool() -> ListToolsTool {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(StubTool {
            name: "browse",
            description: "drive a web browser",
            cap: None,
        }));
        reg.register(Arc::new(StubTool {
            name: "read_calendar",
            description: "list calendar events",
            cap: Some(Capability::new(Action::Read, Resource::domain("calendar"))),
        }));
        ListToolsTool { registry: reg }
    }

    #[tokio::test]
    async fn list_tools_catalog_is_capability_filtered_sorted_and_filterable() {
        let tool = list_tool();

        // Unrestricted context: both tools, sorted, names + descriptions only.
        let out = tool
            .invoke(json!({}), &ToolContext::default())
            .await
            .unwrap();
        assert_eq!(out["count"], json!(2));
        assert_eq!(out["catalog"][0]["name"], json!("browse"));
        assert_eq!(out["catalog"][1]["name"], json!("read_calendar"));
        assert!(out["catalog"][0].get("parameters").is_none());

        // A `filter` substring matches names and descriptions, case-insensitively.
        let out = tool
            .invoke(json!({ "filter": "CALENDAR" }), &ToolContext::default())
            .await
            .unwrap();
        assert_eq!(out["count"], json!(1));
        assert_eq!(out["catalog"][0]["name"], json!("read_calendar"));

        // A context without calendar:read never sees the gated tool (§19).
        let restricted = ToolContext {
            capabilities: Some(vec![Capability::new(
                Action::Write,
                Resource::domain("notes"),
            )]),
            ..Default::default()
        };
        let out = tool.invoke(json!({}), &restricted).await.unwrap();
        assert_eq!(out["count"], json!(1));
        assert_eq!(out["catalog"][0]["name"], json!("browse"));
    }

    #[tokio::test]
    async fn list_tools_names_returns_full_specs_and_reports_the_rest() {
        let tool = list_tool();

        // Exact selection returns the full schema under the `"tools"` key (the
        // deferred-advertising widening protocol, SOUL §7)…
        let out = tool
            .invoke(
                json!({ "names": ["browse", "nope"] }),
                &ToolContext::default(),
            )
            .await
            .unwrap();
        assert_eq!(out["tools"][0]["name"], json!("browse"));
        assert!(out["tools"][0]["parameters"]["properties"]["x"].is_object());
        assert_eq!(out["unknown"], json!(["nope"]));

        // …and a gated tool the caller can't dispatch is "unknown" to it too.
        let restricted = ToolContext {
            capabilities: Some(vec![Capability::new(
                Action::Write,
                Resource::domain("notes"),
            )]),
            ..Default::default()
        };
        let out = tool
            .invoke(json!({ "names": ["read_calendar"] }), &restricted)
            .await
            .unwrap();
        assert_eq!(out["tools"], json!([]));
        assert_eq!(out["unknown"], json!(["read_calendar"]));
    }

    fn model(id: &str, name: &str) -> ModelInfo {
        ModelInfo {
            id: id.to_string(),
            name: name.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn filter_model_catalog_matches_id_and_name_ranked() {
        let catalog = vec![
            model("mistral-small", "Mistral Small"),
            model("claude-haiku-4-5", "Claude Haiku 4.5"),
            model("gpt-4o-mini", "GPT-4o mini (haiku-class)"),
            model("haiku", "Bare Haiku"),
        ];

        // Case-insensitive over ids AND display names; exact id first, then id
        // substring, then name-only matches — each group sorted by id.
        let hits: Vec<String> = filter_model_catalog(catalog.clone(), Some("HaIkU"))
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(hits, vec!["haiku", "claude-haiku-4-5", "gpt-4o-mini"]);

        // No query (or blank) browses the whole catalog, sorted by id.
        let all: Vec<String> = filter_model_catalog(catalog.clone(), None)
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(
            all,
            vec!["claude-haiku-4-5", "gpt-4o-mini", "haiku", "mistral-small"]
        );
        assert_eq!(filter_model_catalog(catalog.clone(), Some("  ")).len(), 4);

        // A query matching nothing yields nothing (never the full catalog).
        assert!(filter_model_catalog(catalog, Some("nope")).is_empty());
    }

    #[test]
    fn parse_model_kind_defaults_to_chat_and_rejects_unknown() {
        assert_eq!(parse_model_kind(None).unwrap(), ModelKind::Chat);
        assert_eq!(parse_model_kind(Some("llm")).unwrap(), ModelKind::Chat);
        assert_eq!(parse_model_kind(Some("tts")).unwrap(), ModelKind::Tts);
        assert_eq!(parse_model_kind(Some("stt")).unwrap(), ModelKind::Stt);
        assert_eq!(
            parse_model_kind(Some("embedding")).unwrap(),
            ModelKind::Embedding
        );
        assert_eq!(parse_model_kind(Some("all")).unwrap(), ModelKind::All);
        assert!(parse_model_kind(Some("images")).is_err());
    }
}
