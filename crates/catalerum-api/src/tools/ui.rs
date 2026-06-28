//! Emerged-UI tools (present/read/list/delete + schema).

use super::*;

/// Current `UiSpec` JSONB format version stamped on newly-authored UIs.
pub(crate) const UI_SPEC_VERSION: u32 = 1;

/// Whether a tool result represents a persisted App definition that chat should
/// mount inline instead of rendering as an ordinary JSON tool card.
pub(crate) fn is_ui_authoring_tool(name: &str) -> bool {
    matches!(
        name,
        "present_ui" | "create_ui_components" | "edit_ui_components" | "edit_ui"
    )
}

// Existing-App tool schemas deliberately avoid a root-level `anyOf`. Moonshot's
// tool-schema normalizer moves the root `type` into the union branches, but its
// API also requires `parameters.type` to remain `object`, so such schemas make
// the entire follow-up request fail. The descriptions still make the target
// contract explicit, and each invoke path enforces that either `id` or `name`
// is supplied before touching the store.

/// Parse the `definition` argument into a [`UiSpec`] and validate it. Rejects
/// unknown node kinds, dangling references, oversized trees, function-like
/// binding paths, and — for `Tool` handlers — any tool not on the server
/// allow-list `allow`. In v1 the allow-list is also the "known tool" set: a
/// handler tool must be allow-listed regardless, so a stricter registry check
/// would not change the accept/reject outcome.
pub(crate) fn parse_ui_spec(args: &Json, allow: &HashSet<String>) -> Result<UiSpec> {
    let raw = args
        .get("definition")
        .ok_or_else(|| Error::invalid("`definition` is required"))?;
    let spec: UiSpec = serde_json::from_value(raw.clone())
        .map_err(|e| Error::invalid(format!("invalid ui definition: {e}")))?;
    validate_ui_spec(&spec, |t| allow.contains(t), |t| allow.contains(t))?;
    Ok(spec)
}

/// The deliberately tiny App shell created when `present_ui` omits a full
/// definition. The model can then grow `root` through `create_ui_components`,
/// one independently-sized section at a time.
fn starter_ui_spec(title: &str) -> UiSpec {
    UiSpec {
        default_view: "main".to_string(),
        views: vec![UiView {
            id: "main".to_string(),
            title: title.to_string(),
            root: UiNode {
                id: "root".to_string(),
                kind: NodeKind::Stack,
                props: Map::new(),
                children: Vec::new(),
                bind: None,
                show_if: None,
                for_each: None,
                events: BTreeMap::new(),
                validate: Vec::new(),
            },
        }],
        initial_state: Map::new(),
        computed: Vec::new(),
        scripts: BTreeMap::new(),
        parent_app: None,
    }
}

/// Parse a `UiDefinitionId` from the `id` argument.
pub(crate) fn ui_def_id(args: &Json) -> Result<UiDefinitionId> {
    required_str(args, "id")?
        .parse::<UiDefinitionId>()
        .map_err(|e| Error::invalid(format!("invalid ui id: {e}")))
}

/// Parse the `patch` argument into a non-empty, ordered list of [`UiPatchOp`]s
/// (the id-targeted edit vocabulary). Unknown ops / malformed payloads are
/// rejected at deserialize (the enum is closed).
pub(crate) fn parse_ui_patch(args: &Json) -> Result<Vec<UiPatchOp>> {
    let raw = args
        .get("patch")
        .ok_or_else(|| Error::invalid("`patch` is required (a list of edit ops)"))?;
    let ops: Vec<UiPatchOp> = serde_json::from_value(raw.clone())
        .map_err(|e| Error::invalid(format!("invalid ui patch: {e}")))?;
    if ops.is_empty() {
        return Err(Error::invalid("`patch` must contain at least one op"));
    }
    Ok(ops)
}

/// `present_ui` — create (or update-by-name) an emerged UI and return its id so
/// the chat layer can mount it inline. A new App may omit `definition` to create
/// a tiny shell for staged component authoring.
pub(crate) struct PresentUiTool {
    pub(crate) uis: UiDefinitionRepo,
    pub(crate) allow: Arc<HashSet<String>>,
}

#[async_trait]
impl Tool for PresentUiTool {
    fn name(&self) -> &str {
        "present_ui"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "ui")
    }

    fn description(&self) -> &str {
        "Create an interactive App and surface it inline in chat. For a non-trivial \
         App, OMIT `definition`: this creates a tiny `main` view with an empty \
         stack root named `root`; then call create_ui_components one or more times \
         to build it in small sections. On EVERY later authoring call, pass either \
         top-level `id` = the returned `ui_id`, or the same stable `name`; neither \
         is inherited implicitly. Use edit_ui's `set_initial_state`/view ops before \
         components that reference state or additional views. A supplied `definition` creates a \
         complete UiSpec. If `name` already exists, its definition is replaced only \
         when a new one is supplied; omitting it keeps the tree and updates metadata. \
         Call explain_ui_schema BEFORE get_ui_schema and use only component names it \
         returned; do not guess names. Returns the ui id, version, default view, root \
         id, and a copy-ready target for the next call."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Optional stable slug (unique per workspace). Omit for a one-off inline UI; set it to update the same UI on a later call." },
                "title": { "type": "string", "description": "Human title shown as the UI header." },
                "description": { "type": "string", "description": "Optional short description." },
                "definition": { "type": "object", "description": "Optional complete UiSpec. Omit for a staged build: the App starts as main/root, then create_ui_components attaches small component subtrees." }
            },
            "required": ["title"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let author = author(ctx)?;
        let title = required_str(&args, "title")?;
        let name = opt_str_some(&args, "name");
        let description = opt_str_some(&args, "description");
        // Upsert by name: patch an existing same-named UI optimistically,
        // otherwise create. Anonymous (no-name) UIs are always created fresh.
        let def = match name.as_deref() {
            Some(n) => match self.uis.get_by_name(ws, n).await {
                Ok(existing) => {
                    // No definition on an existing named App is a metadata-only
                    // update, never an accidental reset to the starter shell.
                    let spec = if args.get("definition").is_some() {
                        parse_ui_spec(&args, &self.allow)?
                    } else {
                        existing.definition.clone()
                    };
                    let input = UiDefinitionInput {
                        name: name.clone(),
                        title,
                        description,
                        definition: spec,
                    };
                    self.uis
                        .update_definition(ws, existing.id, existing.version, &input)
                        .await?
                }
                Err(StoreError::NotFound) => {
                    let spec = if args.get("definition").is_some() {
                        parse_ui_spec(&args, &self.allow)?
                    } else {
                        starter_ui_spec(&title)
                    };
                    let input = UiDefinitionInput {
                        name: name.clone(),
                        title,
                        description,
                        definition: spec,
                    };
                    self.uis.create(ws, author, UI_SPEC_VERSION, &input).await?
                }
                Err(e) => return Err(e.into()),
            },
            None => {
                let spec = if args.get("definition").is_some() {
                    parse_ui_spec(&args, &self.allow)?
                } else {
                    starter_ui_spec(&title)
                };
                let input = UiDefinitionInput {
                    name,
                    title,
                    description,
                    definition: spec,
                };
                self.uis.create(ws, author, UI_SPEC_VERSION, &input).await?
            }
        };
        let root_id = def
            .definition
            .views
            .iter()
            .find(|view| view.id == def.definition.default_view)
            .map(|view| view.root.id.as_str());
        Ok(json!({
            "ui_id": def.id,
            "version": def.version,
            "name": def.name,
            "title": def.title,
            "default_view": def.definition.default_view,
            "root_id": root_id,
            "next_call_target": { "id": def.id },
            "next": "Pass next_call_target.id as top-level `id` on every create_ui_components/edit_ui_components/edit_ui call (or reuse the returned stable `name`).",
            "advertise_tools": ["create_ui_components", "edit_ui", "explain_ui_schema", "get_ui_schema"],
        }))
    }
}

/// Parse the compact append vocabulary used by `create_ui_components` into the
/// existing, closed `insert_node` patch representation. Entries apply in order,
/// so a later component may target a parent inserted earlier in the same call.
fn parse_ui_components(args: &Json) -> Result<(Vec<UiPatchOp>, Vec<String>)> {
    let raw = args
        .get("components")
        .and_then(Json::as_array)
        .ok_or_else(|| Error::invalid("`components` is required (a non-empty array)"))?;
    if raw.is_empty() {
        return Err(Error::invalid(
            "`components` must contain at least one component",
        ));
    }
    if raw.len() > 64 {
        return Err(Error::invalid(
            "`components` accepts at most 64 components per call; use another call",
        ));
    }

    let mut ops = Vec::with_capacity(raw.len());
    let mut ids = Vec::with_capacity(raw.len());
    for (position, component) in raw.iter().enumerate() {
        let parent_id = component
            .get("parent_id")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                Error::invalid(format!(
                    "components[{position}].parent_id must be a non-empty node id"
                ))
            })?
            .to_string();
        let index = match component.get("index") {
            Some(value) => Some(
                value
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or_else(|| {
                        Error::invalid(format!(
                            "components[{position}].index must be a non-negative integer"
                        ))
                    })?,
            ),
            None => None,
        };
        let node: UiNode =
            serde_json::from_value(component.get("node").cloned().ok_or_else(|| {
                Error::invalid(format!("components[{position}].node is required"))
            })?)
            .map_err(|e| Error::invalid(format!("invalid components[{position}].node: {e}")))?;
        ids.push(node.id.clone());
        ops.push(UiPatchOp::InsertNode {
            parent_id,
            index,
            node: Box::new(node),
        });
    }
    Ok((ops, ids))
}

/// `create_ui_components` — append one or more component subtrees to an App in
/// a compact, atomic call, avoiding a full UiSpec or general patch document.
pub(crate) struct CreateUiComponentsTool {
    pub(crate) uis: UiDefinitionRepo,
    pub(crate) allow: Arc<HashSet<String>>,
}

#[async_trait]
impl Tool for CreateUiComponentsTool {
    fn name(&self) -> &str {
        "create_ui_components"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "ui")
    }

    fn description(&self) -> &str {
        "Create one or more subcomponents inside an existing App without sending \
         the whole UiSpec or a general edit patch. REQUIRED TARGET: pass top-level \
         `id` (copy `ui_id` from present_ui) OR top-level `name` on EVERY call; \
         `components` is BESIDE that target. `parent_id` is only a node target \
         inside the App and does not identify the App. Exact envelope: \
         {\"id\":\"<ui_id>\",\"components\":[{\"parent_id\":\"root\",\"node\":{\"id\":\"intro\",\"kind\":\"text\"}}]}. \
         Each ordered entry gives a `parent_id`, optional insertion `index`, \
         and one `node` subtree. Call repeatedly with small logical sections. A \
         later entry may attach beneath a component created earlier in the same \
         call. The entire call is atomic and the resulting App is fully validated. \
         Use only component kinds returned by explain_ui_schema/get_ui_schema. \
         Use edit_ui_components to replace existing logical components; use \
         edit_ui for fine-grained changes/removals or initial state, scripts, \
         computed values, and views."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "minLength": 1, "description": "TOP-LEVEL and REQUIRED unless top-level `name` is supplied. Copy the `ui_id` returned by present_ui. Example: {\"id\":\"<ui_id>\",\"components\":[...]}" },
                "name": { "type": "string", "minLength": 1, "description": "TOP-LEVEL and REQUIRED unless top-level `id` is supplied. Reuse the exact stable name passed to present_ui. Example: {\"name\":\"recipe-box\",\"components\":[...]}" },
                "components": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 64,
                    "description": "Ordered component insertions. This array is a SIBLING of top-level `id` or `name`; nested parent_id values identify nodes only. Keep each call to one or a few logical sections instead of sending the whole App.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "parent_id": { "type": "string", "description": "Existing container node id, or a container inserted earlier in this call (the staged App shell starts with `root`)." },
                            "index": { "type": "integer", "minimum": 0, "description": "Optional child position; omitted appends." },
                            "node": { "type": "object", "description": "One UiNode subtree: {id, kind, props?, children?, bind?, show_if?, for_each?, events?, validate?}." }
                        },
                        "required": ["parent_id", "node"]
                    }
                }
            },
            "required": ["components"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let existing = if args.get("id").and_then(Json::as_str).is_some() {
            self.uis.get(ws, ui_def_id(&args)?).await?
        } else if let Some(name) = opt_str_some(&args, "name") {
            self.uis.get_by_name(ws, &name).await?
        } else {
            return Err(Error::invalid(
                "missing App target: add top-level `id` or top-level `name`: use {\"id\":\"<ui_id>\",\"components\":[...]} or {\"name\":\"<stable-name>\",\"components\":[...]}; `parent_id` only identifies a node. Do not retry unchanged"
            ));
        };
        let (ops, component_ids) = parse_ui_components(&args)?;

        let mut spec = existing.definition.clone();
        apply_ui_patch(&mut spec, &ops)?;
        validate_ui_spec(
            &spec,
            |t| self.allow.contains(t),
            |t| self.allow.contains(t),
        )?;
        let input = UiDefinitionInput {
            name: existing.name.clone(),
            title: existing.title.clone(),
            description: existing.description.clone(),
            definition: spec,
        };
        let def = self
            .uis
            .update_definition(ws, existing.id, existing.version, &input)
            .await?;
        Ok(json!({
            "ui_id": def.id,
            "version": def.version,
            "name": def.name,
            "title": def.title,
            "created": component_ids,
            "next_call_target": { "id": def.id },
        }))
    }
}

/// Parse the compact replacement vocabulary used by `edit_ui_components` into
/// the existing, closed `replace_node` patch representation. Replacements apply
/// in order and are committed only after the final App validates.
fn parse_ui_component_edits(args: &Json) -> Result<(Vec<UiPatchOp>, Vec<String>)> {
    let raw = args
        .get("components")
        .and_then(Json::as_array)
        .ok_or_else(|| Error::invalid("`components` is required (a non-empty array)"))?;
    if raw.is_empty() {
        return Err(Error::invalid(
            "`components` must contain at least one component edit",
        ));
    }
    if raw.len() > 64 {
        return Err(Error::invalid(
            "`components` accepts at most 64 edits per call; use another call",
        ));
    }

    let mut ops = Vec::with_capacity(raw.len());
    let mut ids = Vec::with_capacity(raw.len());
    for (position, component) in raw.iter().enumerate() {
        let node_id = component
            .get("node_id")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                Error::invalid(format!(
                    "components[{position}].node_id must be a non-empty node id"
                ))
            })?
            .to_string();
        let node: UiNode =
            serde_json::from_value(component.get("node").cloned().ok_or_else(|| {
                Error::invalid(format!("components[{position}].node is required"))
            })?)
            .map_err(|e| Error::invalid(format!("invalid components[{position}].node: {e}")))?;
        ids.push(node_id.clone());
        ops.push(UiPatchOp::ReplaceNode {
            node_id,
            node: Box::new(node),
        });
    }
    Ok((ops, ids))
}

/// `edit_ui_components` — replace one or more existing component subtrees in a
/// compact, atomic call, avoiding a full UiSpec or general patch document.
pub(crate) struct EditUiComponentsTool {
    pub(crate) uis: UiDefinitionRepo,
    pub(crate) allow: Arc<HashSet<String>>,
}

#[async_trait]
impl Tool for EditUiComponentsTool {
    fn name(&self) -> &str {
        "edit_ui_components"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "ui")
    }

    fn description(&self) -> &str {
        "Replace one or more existing component subtrees inside an App without \
         sending the whole UiSpec or a general edit patch. REQUIRED TARGET: pass \
         top-level `id` (copy `ui_id` from present_ui/read_ui) OR top-level `name` \
         on EVERY call, with `components` BESIDE it. Exact envelope: \
         {\"id\":\"<ui_id>\",\"components\":[{\"node_id\":\"card\",\"node\":{\"id\":\"card\",\"kind\":\"card\"}}]}. \
         `node_id` does not identify the App. Each ordered entry \
         gives the existing `node_id` and its \
         replacement `node`. Call read_ui with `node_id` first, then send back only \
         that logical component. Replacements may keep or change the node id, apply \
         in order, and commit atomically after the complete App validates. Use \
         edit_ui for tiny property/event changes or structural remove/move operations."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "minLength": 1, "description": "TOP-LEVEL and REQUIRED unless top-level `name` is supplied. Copy the `ui_id` from present_ui/read_ui. Example: {\"id\":\"<ui_id>\",\"components\":[...]}" },
                "name": { "type": "string", "minLength": 1, "description": "TOP-LEVEL and REQUIRED unless top-level `id` is supplied. Reuse the exact stable App name. Example: {\"name\":\"recipe-box\",\"components\":[...]}" },
                "components": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 64,
                    "description": "Ordered component-subtree replacements. This array is a SIBLING of top-level `id` or `name`; nested node_id values identify components only. Keep each call to one or a few logical components instead of sending the whole App.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "node_id": { "type": "string", "description": "Id of the existing component root to replace." },
                            "node": { "type": "object", "description": "Replacement UiNode subtree: {id, kind, props?, children?, bind?, show_if?, for_each?, events?, validate?}." }
                        },
                        "required": ["node_id", "node"]
                    }
                }
            },
            "required": ["components"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let existing = if args.get("id").and_then(Json::as_str).is_some() {
            self.uis.get(ws, ui_def_id(&args)?).await?
        } else if let Some(name) = opt_str_some(&args, "name") {
            self.uis.get_by_name(ws, &name).await?
        } else {
            return Err(Error::invalid(
                "missing App target: add top-level `id` or top-level `name`: use {\"id\":\"<ui_id>\",\"components\":[...]} or {\"name\":\"<stable-name>\",\"components\":[...]}; `node_id` only identifies a component. Do not retry unchanged"
            ));
        };
        let (ops, component_ids) = parse_ui_component_edits(&args)?;

        let mut spec = existing.definition.clone();
        apply_ui_patch(&mut spec, &ops)?;
        validate_ui_spec(
            &spec,
            |t| self.allow.contains(t),
            |t| self.allow.contains(t),
        )?;
        let input = UiDefinitionInput {
            name: existing.name.clone(),
            title: existing.title.clone(),
            description: existing.description.clone(),
            definition: spec,
        };
        let def = self
            .uis
            .update_definition(ws, existing.id, existing.version, &input)
            .await?;
        Ok(json!({
            "ui_id": def.id,
            "version": def.version,
            "name": def.name,
            "title": def.title,
            "edited": component_ids,
            "next_call_target": { "id": def.id },
        }))
    }
}

/// `edit_ui` — partially edit an existing emerged UI by applying an ordered
/// list of id-targeted [`UiPatchOp`]s, then re-validating and persisting the
/// result (optimistic on the current `version`). The surgical alternative to
/// re-sending the whole tree with `present_ui`.
pub(crate) struct EditUiTool {
    pub(crate) uis: UiDefinitionRepo,
    pub(crate) allow: Arc<HashSet<String>>,
}

#[async_trait]
impl Tool for EditUiTool {
    fn name(&self) -> &str {
        "edit_ui"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "ui")
    }

    fn description(&self) -> &str {
        "Partially edit an existing emerged UI, changing only the parts you name \
         instead of re-sending the whole tree. REQUIRED TARGET: pass top-level \
         `id` (copy `ui_id` from present_ui/read_ui) OR top-level `name` on EVERY call, \
         then pass `patch` BESIDE that target, never inside it. Minimal shape: \
         {\"id\":\"<ui_id>\",\"patch\":[{\"op\":\"set_props\",\"node_id\":\"save\",\"props\":{\"label\":\"Save\"},\"merge\":true}]}. \
         `patch` is an ordered list of node/spec-targeted ops (set_props, \
         insert_node, remove_node, move_node, replace_node, set_bind, set_show_if, \
         set_for_each, set_event, set_validate, set_script, set_computed, add_view, \
         remove_view, set_view_root, set_initial_state, set_meta). Call read_ui first (scoped: \
         outline/view/node_id) to see the current node ids, get_ui_schema with \
         topic `editing` for patch ops, and request the component kinds you touch. \
         IMPORTANT: add_view/set_view_root `root` is a COMPLETE UiNode object such as \
         {\"id\":\"detail_root\",\"kind\":\"stack\"}, never a node-id string. \
         To replace a whole logical component subtree, prefer edit_ui_components. \
         Optionally also updates \
         `title`/`description`. The whole result is re-validated before it is \
         saved; the edit is rejected if it would break the spec. Returns the ui id \
         and the new version (re-read and retry on a version conflict)."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "minLength": 1, "description": "TOP-LEVEL and REQUIRED unless top-level `name` is supplied. Copy the `ui_id` from present_ui/read_ui. Example envelope: {\"id\":\"<ui_id>\",\"patch\":[...]}" },
                "name": { "type": "string", "minLength": 1, "description": "TOP-LEVEL and REQUIRED unless top-level `id` is supplied. Reuse the exact stable App name. Example envelope: {\"name\":\"recipe-box\",\"patch\":[...]}" },
                "title": { "type": "string", "description": "Optional new title (omit to keep the current one)." },
                "description": { "type": "string", "description": "Optional new description (empty string clears it; omit to keep)." },
                "patch": {
                    "type": "array",
                    "description": "Ordered node/spec-level edit ops. This array is a SIBLING of top-level `id` or `name`. Each item has an `op` tag, e.g. { \"op\": \"set_initial_state\", \"state\": { \"recipes\": [] }, \"merge\": true }, { \"op\": \"set_props\", \"node_id\": \"save\", \"props\": { \"label\": \"Submit\" }, \"merge\": true }, { \"op\": \"insert_node\", \"parent_id\": \"root\", \"node\": { \"id\": \"note\", \"kind\": \"text\", \"props\": { \"text\": \"Hi\" } } }, or { \"op\": \"add_view\", \"view\": { \"id\": \"detail\", \"title\": \"Detail\", \"root\": { \"id\": \"detail_root\", \"kind\": \"stack\" } } }. A view `root` is a complete node object, NOT a string. Request topic `editing` from get_ui_schema for every op.",
                    "items": { "type": "object" }
                }
            },
            "required": ["patch"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        // Resolve the target UI by id or name (mirrors read_ui).
        let existing = if args.get("id").and_then(Json::as_str).is_some() {
            self.uis.get(ws, ui_def_id(&args)?).await?
        } else if let Some(name) = opt_str_some(&args, "name") {
            self.uis.get_by_name(ws, &name).await?
        } else {
            return Err(Error::invalid(
                "missing App target: add top-level `id` or top-level `name`: use {\"id\":\"<ui_id>\",\"patch\":[...]} or {\"name\":\"<stable-name>\",\"patch\":[...]}. Do not retry unchanged"
            ));
        };
        let ops = parse_ui_patch(&args)?;

        // Apply the ops to a working clone, then re-validate the WHOLE result
        // (unique ids, container rules, reference integrity, size clamps) exactly
        // like present_ui does — a rejected patch never reaches the store.
        let mut spec = existing.definition.clone();
        apply_ui_patch(&mut spec, &ops)?;
        validate_ui_spec(
            &spec,
            |t| self.allow.contains(t),
            |t| self.allow.contains(t),
        )?;

        // Metadata edits are optional: an omitted `title`/`description` keeps the
        // stored value; an explicit empty/absent-string `description` clears it.
        let title = opt_str_some(&args, "title").unwrap_or_else(|| existing.title.clone());
        let description = if args.get("description").is_some() {
            opt_str_some(&args, "description")
        } else {
            existing.description.clone()
        };
        let input = UiDefinitionInput {
            name: existing.name.clone(),
            title,
            description,
            definition: spec,
        };
        // Optimistic on the version we read — a concurrent edit yields Conflict.
        let def = self
            .uis
            .update_definition(ws, existing.id, existing.version, &input)
            .await?;
        Ok(json!({
            "ui_id": def.id,
            "version": def.version,
            "name": def.name,
            "title": def.title,
            "next_call_target": { "id": def.id },
        }))
    }
}

/// `read_ui` — fetch one emerged UI by id or name: the full definition, or a
/// scoped fragment of it (`outline` skeleton / one `view` / one `node_id`
/// subtree) so a large App can be edited without ever pulling the whole tree.
pub(crate) struct ReadUiTool {
    pub(crate) uis: UiDefinitionRepo,
}

/// Compact skeleton of one node: id + kind (+ the composition target for
/// `view_ref`/`app_ref`, so an outline shows how the "files" wire together),
/// recursing into children. Everything else (props, events, bindings) is
/// deliberately dropped — the outline exists to locate patch targets cheaply.
fn outline_node(node: &UiNode) -> Json {
    let mut out = serde_json::Map::new();
    out.insert("id".into(), json!(node.id));
    out.insert(
        "kind".into(),
        serde_json::to_value(node.kind).unwrap_or(Json::Null),
    );
    match node.kind {
        NodeKind::ViewRef => {
            if let Some(v) = node.props.get("view") {
                out.insert("view".into(), v.clone());
            }
        }
        NodeKind::AppRef => {
            if let Some(a) = node.props.get("app") {
                out.insert("app".into(), a.clone());
            }
        }
        _ => {}
    }
    if !node.children.is_empty() {
        out.insert(
            "children".into(),
            Json::Array(node.children.iter().map(outline_node).collect()),
        );
    }
    Json::Object(out)
}

/// Immutable lookup of the node with `id` across every view of `spec`.
fn spec_find_node<'a>(spec: &'a UiSpec, id: &str) -> Option<&'a UiNode> {
    fn find<'a>(node: &'a UiNode, id: &str) -> Option<&'a UiNode> {
        if node.id == id {
            return Some(node);
        }
        node.children.iter().find_map(|c| find(c, id))
    }
    spec.views.iter().find_map(|v| find(&v.root, id))
}

#[async_trait]
impl Tool for ReadUiTool {
    fn name(&self) -> &str {
        "read_ui"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "ui")
    }

    fn description(&self) -> &str {
        "Fetch one emerged UI by `id` (or by `name`). By default returns the \
         full definition; on a LARGE App prefer a scoped read: `outline: true` \
         returns a cheap skeleton (view list + nested node ids/kinds, script and \
         computed names — no props) to locate targets, `view` returns one view's \
         subtree, `node_id` one node's subtree. Use before patching to see the \
         current node ids. The App selector is top-level: {\"id\":\"<ui_id>\",\"outline\":true} \
         or {\"name\":\"recipe-box\",\"node_id\":\"card\"}."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "minLength": 1, "description": "TOP-LEVEL and REQUIRED unless top-level `name` is supplied. Id of the UI to read." },
                "name": { "type": "string", "minLength": 1, "description": "TOP-LEVEL and REQUIRED unless top-level `id` is supplied. The UI's stable name slug." },
                "outline": { "type": "boolean", "description": "Return only a skeleton (nested node ids/kinds per view + script/computed names) instead of the full definition — the cheap first look at a big App." },
                "view": { "type": "string", "description": "Return only this view (by view id): its full subtree plus the spec-level meta. The per-'file' read." },
                "node_id": { "type": "string", "description": "Return only this node's subtree (searched across all views). Takes precedence over `view`/`outline`." }
            }
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let def = if args.get("id").and_then(Json::as_str).is_some() {
            self.uis.get(ws, ui_def_id(&args)?).await?
        } else if let Some(name) = opt_str_some(&args, "name") {
            self.uis.get_by_name(ws, &name).await?
        } else {
            return Err(Error::invalid(
                "missing App target: add top-level `id` or top-level `name`: use {\"id\":\"<ui_id>\"} or {\"name\":\"<stable-name>\"}. Do not retry unchanged"
            ));
        };
        let spec = &def.definition;
        // Shared header for every scoped shape: enough to keep editing (id +
        // version for the optimistic patch, the view list to navigate "files").
        let meta = json!({
            "ui_id": def.id,
            "name": def.name,
            "title": def.title,
            "version": def.version,
            "default_view": spec.default_view,
            "views": spec.views.iter().map(|v| json!({ "id": v.id, "title": v.title })).collect::<Vec<_>>(),
        });
        let mut out = meta;

        if let Some(node_id) = opt_str_some(&args, "node_id") {
            let node = spec_find_node(spec, &node_id).ok_or_else(|| {
                Error::invalid(format!(
                    "node `{node_id}` not found in this ui (read with outline:true to see the node ids)"
                ))
            })?;
            out["node"] = serde_json::to_value(node)?;
        } else if let Some(view_id) = opt_str_some(&args, "view") {
            let view = spec.views.iter().find(|v| v.id == view_id).ok_or_else(|| {
                Error::invalid(format!(
                    "view `{view_id}` not found in this ui (the views are listed by outline:true)"
                ))
            })?;
            out["view"] = serde_json::to_value(view)?;
        } else if args.get("outline").and_then(Json::as_bool) == Some(true) {
            out["views"] = Json::Array(
                spec.views
                    .iter()
                    .map(|v| json!({ "id": v.id, "title": v.title, "root": outline_node(&v.root) }))
                    .collect(),
            );
            out["scripts"] = json!(spec.scripts.keys().collect::<Vec<_>>());
            out["computed"] = json!(spec.computed.iter().map(|c| &c.name).collect::<Vec<_>>());
            if let Some(parent) = &spec.parent_app {
                out["parent_app"] = json!(parent);
            }
        } else {
            return Ok(serde_json::to_value(def)?);
        }
        Ok(out)
    }
}

/// `list_uis` — list the workspace's emerged UIs (compact: no definition body).
pub(crate) struct ListUisTool {
    pub(crate) uis: UiDefinitionRepo,
}

#[async_trait]
impl Tool for ListUisTool {
    fn name(&self) -> &str {
        "list_uis"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "ui")
    }

    fn description(&self) -> &str {
        "List the workspace's emerged UIs (most-recently-edited first) as id / \
         name / title / version. Use read_ui to fetch a UI's full definition."
    }

    fn parameters_schema(&self) -> Json {
        json!({ "type": "object", "properties": {} })
    }

    async fn invoke(&self, _args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let uis = self.uis.list_by_workspace(ws).await?;
        let summaries: Vec<Json> = uis
            .into_iter()
            .map(|u| {
                json!({
                    "id": u.id,
                    "name": u.name,
                    "title": u.title,
                    "version": u.version,
                    "updated_at": u.updated_at,
                })
            })
            .collect();
        Ok(json!({ "uis": summaries }))
    }
}

/// `delete_ui` — remove an emerged UI by id (a write, like `delete_note`).
pub(crate) struct DeleteUiTool {
    pub(crate) uis: UiDefinitionRepo,
}

#[async_trait]
impl Tool for DeleteUiTool {
    fn name(&self) -> &str {
        "delete_ui"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "ui")
    }

    fn description(&self) -> &str {
        "Delete an emerged UI by id. Returns the deleted id."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Id of the UI to delete." }
            },
            "required": ["id"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let id = ui_def_id(&args)?;
        self.uis.delete(ws, id).await?;
        Ok(json!({ "deleted": id }))
    }
}

/// The complete emerged-UI authoring reference. The public tools below expose
/// either its small index or a caller-selected subset, rather than returning
/// this whole value on every discovery call.
const UI_COMPONENT_KINDS: &[&str] = &[
    "stack",
    "row",
    "grid",
    "card",
    "dialog",
    "tabs",
    "tab",
    "constrained_box",
    "aspect_ratio",
    "button",
    "text",
    "heading",
    "markdown",
    "divider",
    "image",
    "link",
    "badge",
    "progress_bar",
    "pie_chart",
    "donut_chart",
    "bar_chart",
    "line_chart",
    "area_chart",
    "sparkline",
    "gauge",
    "radar_chart",
    "heatmap",
    "list",
    "table",
    "text_input",
    "textarea",
    "number_input",
    "date_input",
    "select",
    "radio_group",
    "checkbox",
    "slider",
    "timer",
    "stopwatch",
    "view_ref",
    "app_ref",
];

fn ui_schema_reference() -> Json {
    json!({
        "overview": "A UiSpec is { default_view, views:[{id,title,root}], initial_state?, computed?, scripts?, parent_app? }. \
            Each node = { id (unique), kind, props?, children?, bind?, show_if?, for_each?, events?, validate? }. First call explain_ui_schema and use only returned kind names. For a non-trivial App, call present_ui WITHOUT definition to create main/root; copy its ui_id into top-level id on EVERY later call; set initial state/additional views with edit_ui, then grow views in small create_ui_components calls.",
        "node_kinds": {
            "containers": ["stack", "row", "grid", "card", "dialog", "tabs", "tab", "constrained_box", "aspect_ratio", "button"],
            "content": ["text", "heading", "markdown", "divider", "image", "link", "badge", "progress_bar"],
            "charts": ["pie_chart", "donut_chart", "bar_chart", "line_chart", "area_chart", "sparkline", "gauge", "radar_chart", "heatmap"],
            "collections": ["list", "table"],
            "inputs": ["text_input", "textarea", "number_input", "date_input", "select", "radio_group", "checkbox", "slider"],
            "timers": ["timer", "stopwatch"],
            "composition": ["view_ref", "app_ref"]
        },
        "props_by_kind": {
            "stack": "vertical layout container; children render in order",
            "row": "horizontal wrapping layout container; children render in order",
            "grid": "responsive grid layout container; children render in order",
            "card": "container; props.title? renders a card heading",
            "dialog": "overlay container; props.title?; open/close it with client ops open_dialog/close_dialog targeting its id",
            "button": "interactive container; props.label? and/or children; supports click or submit handlers",
            "text": "props.text (interpolatable plain text)",
            "markdown": "props.text (interpolatable Markdown, rendered safely)",
            "divider": "visual separator; no kind-specific props",
            "tabs": "a container whose children are `tab` nodes; renders a header strip + the active panel",
            "tab": "one panel inside `tabs`; props.label is the header text, children are the panel body",
            "constrained_box": "responsive single-child size boundary. Props are numeric CSS pixels: min_width?, max_width?, min_height?, max_height? (each 0–10000; min cannot exceed max); align? = start|center|end|stretch; overflow? = visible|hidden|auto. Use around an image/chart/card when it should not consume the full App width, e.g. {kind:\"constrained_box\",props:{max_width:480,align:\"center\"},children:[{kind:\"image\",…}]}. Accepts at most one child; wrappers may be nested.",
            "aspect_ratio": "responsive single-child ratio frame. props.ratio? = width/height (default 1, range 0.05–20); fit? = contain|cover|fill for image children. Pair with constrained_box to make bounded media, e.g. a max_width:640 wrapper around a ratio:1.777 aspect_ratio around an image. Accepts at most one child.",
            "heading": "level (1-6), text",
            "image": "src (url), alt — src is scheme-checked; http(s)/relative/data:image pass. Two extra sources: (1) workspace files — src \"files://<store>/<path>\" (or \"files:<path>\" on the default store) serves the stored object with the viewer's auth; (2) an external database — a `db` prop { connection, sql, params?, column? } instead of src: the (spec-held) SQL runs against that Postgres connection via the sql_query gates (db:read@<conn> required of the VIEWER) and must return one image cell (bytea, base64 text, or a data: URL); params may use {{paths}} (e.g. a selected row id) and bind as $1,$2,…; only raster formats (png/jpeg/gif/webp) are served. If the DB column already holds an http(s) image URL, skip `db` and just interpolate it into src.",
            "link": "href (url), label (or text), external (bool → new tab) — href is scheme-checked (javascript: rejected)",
            "badge": "text, variant (neutral|info|success|warn|error)",
            "progress_bar": "value (number or {{path}}), max (default 100), label?",
            "charts:common": "leaf nodes (no children/bind/events). Shared props: title?, width?, height?, colors? (CSS-colour array overriding the theme ramp), max? (value-axis ceiling). `data` is a literal array, a {\"$path\":\"state.path\"} ref, or a \"state.path\"/\"{{state.path}}\" string.",
            "pie_chart / donut_chart": "data: [{label,value,color?}] or [number]; legend? (default true). Only positive values are drawn.",
            "bar_chart": "data: [{label,value,color?}] or [number]; horizontal? (bool) flips orientation.",
            "line_chart / area_chart": "data: [{label,value}] or [number] — single series; label is the x-axis tick.",
            "sparkline": "data: [number] — compact, axis-less trend line.",
            "gauge": "value, min (0), max (100) — each a number or {{path}}; title? shown under the dial.",
            "radar_chart": "axes: [\"A\",\"B\",\"C\"] (>=3), data: [number] aligned to axes.",
            "heatmap": "data: [[number,…],…] (rows of numbers); rows?/cols?: string label arrays.",
            "list": "read-only leaf. data (array; literal, {\"$path\":…} or \"{{path}}\"), item? (path within each element to display, e.g. \"title\"), ordered? (bool → <ol>), empty? (text when no rows). For per-row buttons/inputs use for_each instead.",
            "table": "read-only leaf. data (array of objects), columns?: [{header?, path}] or [\"path\"] (omitted → derived from the first row's keys), empty? (text when no rows). For per-row actions use for_each instead.",
            "text_input": "label?, placeholder; bind is required for two-way string state; supports input/change handlers and validate rules",
            "textarea": "multiline text input; label?, placeholder; bind is required for two-way string state; supports input/change handlers and validate rules",
            "number_input": "placeholder, min, max, step — binds a JSON number",
            "date_input": "placeholder, min, max — binds an ISO date string",
            "slider": "min (0), max (100), step (1) — binds a JSON number",
            "select": "options: [\"a\"] or [{value,label}]",
            "radio_group": "options (same as select) — binds the chosen value string",
            "checkbox": "label? — binds a JSON boolean",
            "timer": "a countdown. duration (seconds; number or {{path}}), label?, auto_start? (bool), controls? (bool, default true → Start/Pause/Reset buttons). Fires its `complete` handler once when it reaches zero. Addressable from ANY handler via client ops {op:start_timer|pause_timer|reset_timer, id} — e.g. a recipe step button that starts a 10-minute timer.",
            "stopwatch": "a count-up timer. label?, auto_start?, controls?. Same start/pause/reset ops; no `complete`.",
            "view_ref": "renders another VIEW of this same spec inline: props.view = the view id. Use for shared fragments (a recipe card used in two screens) or to keep one view's tree small. Cycles are rejected at authoring. The embedded view's root `load` handler does NOT fire (load stays a navigation lifecycle).",
            "app_ref": "mounts a whole OTHER emerged UI inline: props.app = its ui id (UUID) OR its `name` slug (both from present_ui/list_uis; names read better in a hand-edited shell). The child runs as itself (own state, own handlers). Cycle/depth guarded (max 4 deep)."
        },
        "binding": {
            "interpolation": "Any string prop may embed {{dotted.path}} read from state (and loop item/index). HTML-escaped; markdown kind renders safely.",
            "bind": "Two-way value binding on input kinds, e.g. \"form.email\".",
            "show_if": "A single state path, optional leading '!'. Falsy: false/null/0/\"\"/[]/{}/absent.",
            "for_each": "{ \"in\": \"items\", \"as\": \"item\", \"index\"?: \"i\", \"key\"?: \"id\", \"filter\"?: {…}, \"filters\"?: [{…}], \"paginate\"?: {…} } repeats the node per array element.",
            "for_each.filter": "{ \"query\": \"search\", \"path\"?: \"title\", \"mode\"?: \"contains\"|\"equals\" } — client-side live row filter: a row passes when the value at `path` (within the item; whole item when unset) matches the state value at `query`. contains = case-insensitive substring (bind a text_input to the query path for live search); equals = exact match (bind a select for a category filter). A falsy query shows every row. `filters` is a LIST of the same shape ANDed with `filter` — e.g. a text search plus a category dropdown filtering the same rows at once.",
            "for_each.paginate": "{ \"page_size\"?: 20, \"mode\"?: \"paged\"|\"infinite\" } — client-side windowing over the (filtered) rows, so only the current window reaches the DOM (the whole array still lives in state; no server round-trip). paged = one fixed-size page at a time under a prev/‹Page X of Y›/next pager. infinite = start with one page and reveal another each time the user scrolls to the bottom (an IntersectionObserver, plus a \"Load more\" fallback). Applies ON TOP of `filter`/`filters` (the pager/scroll act on the filtered set); pagination state resets naturally as filters narrow. Omit for short lists. page_size clamps to [1,200]; infinite scroll stops growing at 1000 rendered rows."
        },
        "events": ["click", "submit", "change", "input", "select", "open", "close", "load", "complete"],
        "lifecycle": "`load` fires when a view becomes active (mount + navigate-to) and only on that view's ACTUAL ROOT node (for a staged App, this is initially the node id `root`). Put the `tool` or `script` handler on that root itself — never on a child shell/stack attached beneath it — to pull durable data into state so the App opens populated, e.g. app_data_list → result_path. A nested `load` is rejected because it would never fire. Mount-time load results are briefly shared across replayed mounts of the same UI version (~30s), so a reopened chat does not re-fire the tool once per transcript line. `change` fires on a COMMITTED input value (text blur/Enter, a select pick, a slider release) — attach a tool/script handler for server-side reactions to edits. `input` fires per keystroke on text-like inputs and slider drags, debounced ~400ms — use it for live server-backed lookups (e.g. a suggestion search); prefer `change` for anything with side effects. `complete` fires once when a `timer` hits zero.",
        "computed": "computed:[{name, handler}] values re-evaluate SERVER-SIDE: on mount, after every handler round-trip, and (debounced ~350ms) whenever a bound input changes — so a servings slider drives {{computed.scaled_amounts}} live without any button.",
        "durable_data": "Persist App data with the app_data_get/set/list/delete tools (a per-App key/value store; the namespace is forced to this App). Pattern: root `load` → app_data_list → result_path \"stored\" → for_each over \"stored.entries\" (each { key, value, updated_at }). To write-then-refresh in one click, use a `script` handler: catalerum.callTool('app_data_set', {key, value}) then catalerum.setState({stored: catalerum.callTool('app_data_list', {})}).",
        "external_db": "For relational App data, discover an attached Postgres database with list_external_database_connections, then read/write it with sql_query (gated db:read@<conn> / db:write@<conn> on the VIEWER); never invent a connection name. A SELECT result has shape {connection,row_count,rows:[…]}. Therefore a load handler with result_path:\"db_result\" must render a collection from \"db_result.rows\", NOT from \"db_result\"; use a script handler if existing nodes require the array copied to another state path. When the user asks for records queried from the database, put the SELECT on the browse view's root load handler so the primary experience is database-backed, rather than adding a disconnected database demo tab. SQL values always use $1/$2 placeholders and params; identifiers such as schema/table names cannot be parameterized or interpolated, so inspect and choose them while authoring. Plain parameter arrays/objects bind as jsonb; for a native PostgreSQL text[] column, pass the parameter as {\"$pg_type\":\"text[]\",\"value\":[\"tag-a\",\"tag-b\"]}. Write example: { kind:\"tool\", tool:\"sql_query\", args:{ connection:\"<name-or-id>\", sql:\"INSERT INTO orders_web (customer, total) VALUES ($1, $2)\", params:[\"{{form.customer}}\", \"{{form.total}}\"] } }. Rows inserted this way can drive automations: a collect_sql trigger polls wildcard-matched tables (e.g. tables:\"orders_*\").",
        "navigation": "For an in-App subpage/detail page, use a separate VIEW, not an external link and not a dialog unless requested. Master-detail pattern: initial_state includes selectedId; each repeated row/card has a button whose client handler first sets selectedId to the loop item's id, then navigates to the detail view; the detail view filters the same state rows by selectedId and includes a button that navigates back to the browse view. Build the detail view BEFORE any button navigates to it, because dangling navigation targets are rejected. Exact shell example: edit_ui {\"name\":\"recipe-box\",\"patch\":[{\"op\":\"add_view\",\"view\":{\"id\":\"detail\",\"title\":\"Recipe detail\",\"root\":{\"id\":\"detail_root\",\"kind\":\"stack\"}}}]}; then attach detail children with create_ui_components using the same top-level App target and parent_id:\"detail_root\".",
        "shell_apps": "Split a large App into a SHELL plus SUB-APPS so each stays small to edit: (1) present_ui each sub-app with a `name` (e.g. recipe-browse, recipe-editor); (2) present_ui the shell whose views embed them via app_ref nodes — by that name or by ui id (tabs work well: one tab per sub-app); (3) update each sub-app adding top-level parent_app: <shell ui id> (parent_app is always the id). When BOTH sides point at each other (shell app_ref→sub by id or name, sub parent_app→shell) the whole suite shares the SHELL's app_data namespace, so sub-apps read/write the same durable rows. A one-sided claim stays isolated. Sub-apps with parent_app are hidden from the Apps panel list (they render inside the shell).",
        "handlers": {
            "client": "{ \"kind\":\"client\", \"ops\":[{op:set,path,value}|{op:toggle,path}|{op:navigate,view}|{op:select_tab,id,index}|{op:open_dialog,id}|{op:close_dialog,id}|{op:append,path,value}|{op:remove_at,path,index}|{op:start_timer,id}|{op:pause_timer,id}|{op:reset_timer,id}] } — local only, no server call. set/append values may be a literal, {\"$path\":\"a.b\"} (copy from a state path), or a string with {{path}} references (a whole reference like \"{{item.id}}\" keeps the raw type — the master-detail pattern: a for_each row button does {op:set,path:selectedId,value:\"{{item.id}}\"} then {op:navigate,view:\"detail\"}).",
            "ai": "{ \"kind\":\"ai\", \"prompt\"?, \"include_state\"? } — sends a new chat turn carrying the event + current state.",
            "tool": "{ \"kind\":\"tool\", \"tool\", \"args\"?, \"result_path\"?, \"then\"? } — invoke an allow-listed tool (args may use {{paths}}).",
            "script": "{ \"kind\":\"script\", \"handler\" } — run a named Boa script from scripts{}."
        },
        "editing": {
            "overview": "Every edit_ui call has the envelope {id:\"<ui_id>\",patch:[...]} or {name:\"<stable-name>\",patch:[...]}; the App target is TOP-LEVEL and is a sibling of patch. To ADD nodes to an existing App, prefer create_ui_components: each call attaches one or a few component subtrees and avoids the general patch envelope. To REPLACE whole logical component subtrees, read each with read_ui {node_id} and pass only those subtrees to edit_ui_components. For tiny property/event edits and structural remove/move operations, use edit_ui's node/spec-targeted patch ops. Every write tool validates the complete result and persists atomically with optimistic versioning.",
            "structure": "For creation, start with present_ui {title,name?} (no definition), then call create_ui_components against parent `root` in small logical sections. For editing, use read_ui {outline:true} to locate a component and read_ui {node_id:\"<id>\"} to fetch only its subtree before edit_ui_components. Treat VIEWS as the App's source files and keep each one small: split shared or bulky fragments into their own views and embed them where needed with `view_ref` nodes (same state, cycle-checked). A whole SUITE of screens splits further into shell + sub-apps (see shell_apps).",
            "ops": {
                "set_props": "{ op, node_id, props, merge? } — replace a node's props, or (merge:true) shallow-merge the given keys into them (e.g. rename a button label).",
                "insert_node": "{ op, parent_id, index?, node } — add a new child node under a container (index omitted = append). `node` is a full node object; its id must be new/unique.",
                "remove_node": "{ op, node_id } — delete a node and its subtree. A view ROOT cannot be removed this way — use remove_view.",
                "move_node": "{ op, node_id, new_parent_id, index? } — reparent a node (index omitted = append). Moving under one's own descendant is rejected.",
                "replace_node": "{ op, node_id, node } — swap a node AND its subtree for `node`, keeping its position: the rewrite-one-fragment op. A view root is a valid target (unlike remove_node); the replacement may keep or change the id.",
                "set_bind": "{ op, node_id, bind } — set an input's two-way state path, or null to clear.",
                "set_show_if": "{ op, node_id, show_if } — set/clear the conditional-render path.",
                "set_for_each": "{ op, node_id, for_each } — set/clear the loop binding (same shape as the node `for_each`).",
                "set_event": "{ op, node_id, event, handler } — set the handler for one event (click/submit/change/…), or null to remove it.",
                "set_validate": "{ op, node_id, rules } — replace an input's validation rules.",
                "set_script": "{ op, name, def } — add/replace a named Boa script, or null to remove it.",
                "set_computed": "{ op, name, def } — add/replace a computed value by name, or null to remove it.",
                "set_initial_state": "{ op, state, merge? } — replace the App's initial_state object; merge:true shallow-merges the supplied top-level keys. Use this immediately after present_ui for static arrays/forms that component bindings or for_each loops will read.",
                "add_view": "{ op:\"add_view\", view:{ id, title, root:{id,kind,props?,children?,events?,...} } } — append a view. `root` is a COMPLETE UiNode object, never an id string. Minimal root: {id:\"detail_root\",kind:\"stack\"}. Full edit_ui example: {name:\"recipe-box\",patch:[{op:\"add_view\",view:{id:\"detail\",title:\"Detail\",root:{id:\"detail_root\",kind:\"stack\"}}}]}",
                "remove_view": "{ op, view_id } — drop a view (cannot leave the spec with zero views, or dangling default_view).",
                "set_view_root": "{ op, view_id, root } — replace a view's entire root node; `root` is a complete UiNode object, never an id string.",
                "set_meta": "{ op, default_view? } — change spec-level metadata (the mount view)."
            }
        },
        "limits": { "max_depth": 32, "max_nodes": 2000, "max_views": 32 },
        "example": {
            "default_view": "main",
            "initial_state": { "spend": [{ "label": "LLM", "value": 42 }, { "label": "TTS", "value": 8 }, { "label": "Embed", "value": 15 }] },
            "views": [{
                "id": "main", "title": "Contact",
                "root": { "id": "root", "kind": "stack", "children": [
                    { "id": "name_lbl", "kind": "text", "props": { "text": "Your name" } },
                    { "id": "name", "kind": "text_input", "bind": "form.name", "props": { "placeholder": "Jane" } },
                    { "id": "save", "kind": "button", "props": { "label": "Save" },
                      "events": { "click": { "kind": "client", "ops": [{ "op": "set", "path": "saved", "value": true }] } } },
                    { "id": "ok", "kind": "text", "show_if": "saved", "props": { "text": "Saved {{form.name}}!" } },
                    { "id": "spend_chart", "kind": "donut_chart",
                      "props": { "title": "Spend", "data": { "$path": "spend" } } }
                ] }
            }]
        }
    })
}

const UI_SCHEMA_TOPICS: &[&str] = &[
    "binding",
    "events",
    "lifecycle",
    "computed",
    "durable_data",
    "external_db",
    "navigation",
    "shell_apps",
    "handlers",
    "editing",
    "limits",
    "example",
];

fn requested_names(args: &Json, key: &str) -> Result<Vec<String>> {
    let values = args
        .get(key)
        .and_then(Json::as_array)
        .ok_or_else(|| Error::invalid(format!("`{key}` must be an array of strings")))?;
    let mut names = Vec::with_capacity(values.len());
    for value in values {
        let name = value
            .as_str()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| Error::invalid(format!("`{key}` must contain non-empty strings")))?;
        if !names.iter().any(|existing| existing == name) {
            names.push(name.to_string());
        }
    }
    Ok(names)
}

fn component_category(reference: &Json, component: &str) -> Option<String> {
    reference["node_kinds"]
        .as_object()?
        .iter()
        .find_map(|(category, kinds)| {
            kinds
                .as_array()?
                .iter()
                .any(|kind| kind.as_str() == Some(component))
                .then(|| category.clone())
        })
}

fn component_props(reference: &Json, component: &str) -> Option<Json> {
    let props = reference["props_by_kind"].as_object()?;
    let exact = props.get(component).cloned();
    let shared = match component {
        "pie_chart" | "donut_chart" => props.get("pie_chart / donut_chart").cloned(),
        "line_chart" | "area_chart" => props.get("line_chart / area_chart").cloned(),
        _ => None,
    };
    let chart =
        component.ends_with("_chart") || matches!(component, "sparkline" | "gauge" | "heatmap");
    let common = chart.then(|| props.get("charts:common").cloned()).flatten();
    let details = [common, exact, shared]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    (!details.is_empty()).then_some(Json::Array(details))
}

/// `explain_ui_schema` — a cheap discovery call listing the available emerged-
/// UI components and optional authoring topics.
pub(crate) struct ExplainUiSchemaTool;

#[async_trait]
impl Tool for ExplainUiSchemaTool {
    fn name(&self) -> &str {
        "explain_ui_schema"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "ui")
    }

    fn description(&self) -> &str {
        "Return a compact overview and grouped list of emerged-UI components. \
         Then call get_ui_schema with only the components and optional topics \
         needed for the UI you are authoring."
    }

    fn parameters_schema(&self) -> Json {
        json!({ "type": "object", "properties": {} })
    }

    async fn invoke(&self, _args: Json, _ctx: &ToolContext) -> Result<Json> {
        let reference = ui_schema_reference();
        Ok(json!({
            "overview": reference["overview"],
            "components": reference["node_kinds"],
            "topics": UI_SCHEMA_TOPICS,
            "next": "Call get_ui_schema with {components:[...]} and, when needed, topics:[...]. Example: {components:[\"stack\",\"text_input\",\"button\"],topics:[\"binding\",\"handlers\"]}."
        }))
    }
}

/// `get_ui_schema` — focused emerged-UI documentation for selected components.
pub(crate) struct GetUiSchemaTool;

#[async_trait]
impl Tool for GetUiSchemaTool {
    fn name(&self) -> &str {
        "get_ui_schema"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "ui")
    }

    fn description(&self) -> &str {
        "Return detailed emerged-UI schema guidance for a requested list of \
         components, plus optional authoring topics. The `components` item schema \
         enumerates every valid name. Prefer calling explain_ui_schema first; use \
         only names it returned. Names such as `chip` or `scroll_view` are invalid."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "components": {
                    "type": "array",
                    "items": { "type": "string", "enum": UI_COMPONENT_KINDS },
                    "minItems": 1,
                    "maxItems": 32,
                    "description": "Component names returned by explain_ui_schema, e.g. stack, text_input, button."
                },
                "topics": {
                    "type": "array",
                    "items": { "type": "string", "enum": UI_SCHEMA_TOPICS },
                    "maxItems": 12,
                    "description": "Optional shared guides returned by explain_ui_schema, e.g. binding or handlers."
                }
            },
            "required": ["components"]
        })
    }

    async fn invoke(&self, args: Json, _ctx: &ToolContext) -> Result<Json> {
        let components = requested_names(&args, "components")?;
        if components.is_empty() {
            return Err(Error::invalid(
                "`components` must contain at least one component",
            ));
        }
        if components.len() > 32 {
            return Err(Error::invalid(
                "`components` accepts at most 32 unique names",
            ));
        }
        let topics = if args.get("topics").is_some() {
            requested_names(&args, "topics")?
        } else {
            Vec::new()
        };
        let reference = ui_schema_reference();
        let mut selected = serde_json::Map::new();
        for component in components {
            let category = component_category(&reference, &component).ok_or_else(|| {
                Error::invalid(format!(
                    "unknown UI component `{component}`. Remove it and retry with only names returned by explain_ui_schema. Valid names: {}",
                    UI_COMPONENT_KINDS.join(", ")
                ))
            })?;
            selected.insert(
                component.clone(),
                json!({
                    "kind": component,
                    "category": category,
                    "props": component_props(&reference, &component).unwrap_or_else(|| json!([]))
                }),
            );
        }
        let mut selected_topics = serde_json::Map::new();
        for topic in topics {
            if !UI_SCHEMA_TOPICS.contains(&topic.as_str()) {
                return Err(Error::invalid(format!(
                    "unknown UI schema topic `{topic}`; call explain_ui_schema for valid names"
                )));
            }
            selected_topics.insert(topic.clone(), reference[&topic].clone());
        }
        Ok(json!({
            "overview": reference["overview"],
            "components": selected,
            "topics": selected_topics
        }))
    }
}

// ---------------------------------------------------------------------------
// Per-App durable key/value store (SOUL §12/§29)
// ---------------------------------------------------------------------------
