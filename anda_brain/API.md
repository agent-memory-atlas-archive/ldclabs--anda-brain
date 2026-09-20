# Anda Brain API Documentation (with TypeScript Types)

Rust now requires Cognitive Nexus 0.13.1: Watch firing atomically records the
transition, `watch_fire` Activity and a protected wake, with replayable receipts
and native fenced leases. The service now schedules structured Watches independently
of Full Maintenance, including registered Spaces evicted from memory. Its durable
catalog resumes bounded scans after restart. Trusted Rust hosts can now install
`ActionBindings` before loading Spaces: four-way decisions, clarification, fixed
Attempts and native fenced dispatch share that scheduler. No production adapter is
installed by default. The runtime API exposes authenticated inbox/response/outcome routes;
`BRAIN_RUNTIME_CONFIG` can install the compiled persistent inbox adapter and explicit
identity/observer mappings. See [runtime setup and recovery](RUNTIME.md).
Spaces cannot fork native attention identities. The internal
`memory_runtime/status` operation reads actual configuration without claiming work
or granting permission; existing HTTP payload shapes and token scopes are unchanged.

Bulk mnemonic decay excludes operational SleepTask/Watch Concepts. Host pass errors
reach the maintenance model as assessment.settlement_errors.

## 1) Common Conventions

- Base URL: `http://{host}:{port}`
- Auth header: `Authorization: Bearer <token>`
- Sharded deployments: send `Shard-Id: <index>` (or `X-Shard`) matching the server's `SHARDING_IDX`; the default is `0`.
- If `ED25519_PUBKEYS` is empty/not provided, authentication is disabled for the legacy endpoints. The new runtime routes always require verified credentials/mappings; independent HTTP outcomes remain disabled without a signed CWT verifier.
- Supported serialization formats:
  - Request: `Content-Type: application/json | application/cbor | text/markdown`
  - Response: `Accept: application/json | application/cbor | text/markdown`
  - Content negotiation applies to success bodies. Handler errors use JSON regardless of `Accept`; middleware load shedding (`429`/`503`) and unmatched routes can return plain text or an empty body.
- Most business endpoints return an RPC envelope: `RpcResponse<T>`
- MCP clients can use the built-in Streamable HTTP endpoint: `/mcp/<space_id>`, or the local stdio server: `anda_brain mcp --space-id <space_id> [local|aws]`

CognitiveMemory 2.1 synchronization preserves existing request shapes, authentication
and JSON/CBOR/Markdown negotiation. No five-intent Memory Interface or standard after
barrier is added; a conversation id is not a processing receipt. Settlement `skills`
adds optional `unsupported_reason` when no trusted learning pipeline is configured;
legacy counters stay zero. Watch `disarmed` also counts Nexus expiry, while text
conditions remain deferred without a configured semantic evaluator. Model-generated Formation requests cannot replace captured
ingest/msgN bindings; learning/runtime Facet writes fail UnsupportedCapability.
Authorized raw administrative KIP remains subject to the engine's full contracts.
Legacy `@2.0.0/Watch` and `@2.0.0/SleepTask` records require explicit 2.1
replacement because their exact `schema_ref` cannot be changed in place.

---

## 2) TypeScript Type Definitions

```ts
export type TokenScope = 'read' | 'write' | '*';

export interface RpcError {
  message: string;
  data?: unknown;
}

export interface RpcResponse<T> {
  result?: T;
  error?: RpcError;
  next_cursor?: string;
}

export interface InputContext {
  counterparty?: string;
  agent?: string;
  source?: string; // provenance thread/channel; not a submission/message deduplication key
  topic?: string;
}

export type MessageRole = 'system' | 'user' | 'assistant' | 'tool';

export type MessageContentPart =
  | string
  | {
      type: string;
      text?: string;
      [k: string]: unknown;
    };

export interface Message {
  role: MessageRole;
  content: string | MessageContentPart[];
  name?: string;  // user or tool name
  user?: string;  // user ID
  timestamp?: number; // Unix timestamp in milliseconds
}

export interface FormationInput {
  messages: Message[]; // must contain at least one non-empty message (400)
  context?: InputContext;
  timestamp?: string; // RFC 3339; normalized to UTC milliseconds, invalid/missing falls back to receipt time
}

export interface RecallInput {
  query: string; // must not be empty/blank (400)
  context?: InputContext;
  budget?: RecallBudget | null;
}

export interface RecallBudget {
  tokenizer?: 'o200k_base@tiktoken-rs-0.12.0';
  max_tokens?: number; // 1–65536; default 4096 after explicit opt-in
  context_tokens?: number; // 1–131072; default 32768; cumulative normalized planner inputs
}

export interface MemoryPolicy {
  version?: number;
  memory_strength_decay_factor?: number;
  recall_reinforcement?: number; // retained for stored-policy compatibility; inert
  correction_penalty?: number; // retained for stored-policy compatibility; inert
  decay_floor?: number;
  stale_event_threshold_days?: number;
  unconsolidated_max_backlog?: number;
  orphan_max_count?: number;
  self_test_queries_per_cycle?: number;
  self_test_token_budget?: number;
  recall_search_threshold?: number; // declared but not consumed yet
  recall_max_rounds?: number;
  recall_budget?: RecallBudget | null;
  shadow_replay_sample?: number;
}

export interface MemoryCitation {
  entity: string;
  type?: string;
  name?: string;
  confidence?: number;
  source?: string;
  created_at?: string;
}

export interface RecallBudgetReceipt {
  tokenizer: string;
  token_limit: number;
  tokens: number;
  context_token_limit: number;
}

export interface RecallOutput {
  answer: string;
  found: boolean;
  uncertainty?: number;
  memories?: MemoryCitation[];
  conversation?: number;
  usage: Usage;
  failed_reason?: string;
  memory_budget?: RecallBudgetReceipt;
}

export interface ProbeInput {
  query: string;
  limit?: number;
}

export interface ProbeOutput {
  found: boolean;
  negative_cached: boolean;
  search_exhaustive?: boolean;
  hits?: MemoryCitation[];
}

export interface MemoryPinInput {
  entity: string; // graph element id such as C-7, P-3, or A-2
  pinned?: boolean; // default true
}

export interface MemoryPinOutput {
  entity: string;
  pinned: boolean;
  updated: number; // number of changed retention records
}

export interface MemoryForgetInput {
  entities: string[];
  dry_run?: boolean; // default false; inspect a dry run before deletion
}

export interface MemoryForgetEntity {
  entity: string;
  existed: boolean;
  error?: string;
}

export interface MemoryForgetReport {
  dry_run: boolean;
  deleted_concepts: number;
  deleted_propositions: number;
  deleted_assertions: number;
  deleted_evidence: number;
  deleted_activities: number;
  entities?: MemoryForgetEntity[];
}

export interface MemoryMetrics {
  recalls_completed: number;
  entities_recalled: number;
  probe_hits: number;
  probe_misses: number;
  negative_cache_hits: number;
  self_test_tested: number;
  self_test_grounded: number;
  reencode_tasks: number;
  corrections: number;
  decayed: number;
  uncertainty_reports: number;
  uncertainty_sum: number;
  forgotten_entities: number;
  updated_at: number;
}

export interface MemoryGraphCounters {
  concepts: number;
  propositions: number;
  unconsolidated?: number;
  orphans?: number;
  predicate_types?: number;
  as_of?: number;
}

export interface WatchSettlement {
  fired: number;
  conflicted: number;
  disarmed: number;
  deferred: number;
  error?: string;
}

export interface SkillSettlement {
  unsupported_reason?: string;
  graded: number;
  transitions: number;
  conflicted: number;
  error?: string;
}

export interface MemorySettlementReport {
  settled_at: number;
  revised_roots?: unknown[];
  decayed: number;
  decay_ran: boolean;
  new_corrections: number;
  watches: WatchSettlement;
  skills: SkillSettlement;
  decay_error?: string;
  correction_scan_error?: string;
  correction_scan_incomplete: boolean;
  correction_scan_through_seq: number;
  retention: {
    expired_assertions: number;
    archived: number;
    held: number;
    refused: number;
    remaining: number;
    error?: string;
  };
}

export interface SelfTestReport {
  tested_at: number;
  tested: number;
  grounded: number;
  reencode_tasks: number;
  usage: Usage;
}

export interface ShadowEvalInput {
  policy: MemoryPolicy;
  replay_sample?: number; // default from policy, at most 16
}

export interface ShadowReport {
  compared_at: number;
  replayed: number;
  baseline_wins: number;
  candidate_wins: number;
  ties: number;
  judge_errors: number;
  candidate_policy: MemoryPolicy;
  usage: Usage;
  samples?: { query: string; winner: 'baseline' | 'candidate' | 'tie' | 'error'; reason?: string }[];
}

export interface MemoryStatus {
  metrics: MemoryMetrics;
  groundability?: number;
  probe_hit_rate?: number;
  correction_rate?: number;
  avg_uncertainty?: number;
  maintenance_tokens_per_recall?: number;
  graph: MemoryGraphCounters;
  last_settlement?: MemorySettlementReport;
  last_self_test?: SelfTestReport;
  last_shadow?: ShadowReport;
  last_schema_audit?: { audited_at: number; predicates?: Record<string, number> };
}

export interface MaintenanceParameters {
  stale_event_threshold_days?: number; // [1, 365]
  memory_strength_decay_factor?: number; // (0, 1]; alias: confidence_decay_factor
  unconsolidated_max_backlog?: number; // [1, 10000]; alias: unsorted_max_backlog
  orphan_max_count?: number; // [1, 10000]
}

export interface MaintenanceInput {
  trigger?: 'scheduled' | 'threshold' | 'on_demand';
  scope?: 'full' | 'quick' | 'daydream'; // defaults to 'daydream'
  timestamp?: string; // canonical UTC: YYYY-MM-DDTHH:mm:ss.SSSZ
  parameters?: MaintenanceParameters;
}

export interface AddSpaceTokenInput {
  scope: TokenScope; // minting "*" requires a "*"-scoped CWT
  name: string; // required, unique per space
  expires_at?: number; // Unix timestamp in milliseconds
  labels?: string[]; // wiki ACL labels; omitted = unrestricted, [] = unlabeled only
}

export interface RevokeSpaceTokenInput {
  token?: string; // full token value…
  name?: string; // …or the unique token name (one of the two is required)
}

export interface UpdateSpaceInput {
  name?: string;
  description?: string;
  public?: boolean;
  wiki_digest?: boolean; // enable WikiDigest graph extraction (default false)
  wiki_audit_reads?: boolean; // event external wiki reads (default false)
  wiki_acl_defaults?: Record<string, string>; // namespace -> default ACL label
  memory_policy?: MemoryPolicy; // replaces the space policy; omitted members use server defaults
}

export interface FormationRestartInput {
  conversation: number;
}

export interface CreateOrUpdateSpaceInput {
  user: string;
  space_id: string;
  tier: number;
}

export interface GetOrInitUserInput {
  user: string;
  name?: string;
}

// ── Wiki: versioned reference documents with verifiable citations ──────────

export type WikiDocStatus = 'active' | 'archived';
export type WikiSearchMode = 'chunks' | 'docs';

export interface WikiCommitInput {
  doc_id?: number; // omit to create a new document
  parent_version?: number; // required on update (CAS); stale value -> 409
  namespace?: string; // default "default"
  slug?: string; // display slug; derived from title when omitted
  title: string;
  content: string; // full Markdown document (not a diff); <= 1 MiB normalized
  tags?: string[]; // omit to keep stored tags on update
  acl_label?: string; // omit to keep/inherit namespace default; "" clears
  source_uri?: string; // omit to keep
  message?: string; // commit message
  metadata?: Record<string, unknown>; // omit to keep
}

export interface WikiDocInfo {
  id: number;
  namespace: string;
  slug: string;
  title: string;
  status: WikiDocStatus;
  current_version: number;
  current_checksum: string; // "sha3-256:..."
  tags: string[];
  acl_label?: string;
  source_uri?: string;
  metadata?: Record<string, unknown>;
  created_by: string;
  updated_by: string;
  created_at: number;
  updated_at: number;
}

export interface WikiVersionInfo {
  id: number;
  doc_id: number;
  parent_version?: number;
  checksum: string;
  size: number;
  author: string;
  message?: string;
  created_at: number;
}

export interface WikiCommitOutput {
  doc: WikiDocInfo;
  version: WikiVersionInfo;
  chunks: number;
  created: boolean;
  idempotent: boolean; // true when nothing changed (no new version written)
}

export interface WikiSearchInput {
  query: string; // BM25 keywords: exact terms, product names, error codes
  namespaces?: string[];
  doc_ids?: number[];
  tags?: string[];
  top_k?: number; // 1-50, default 8
  mode?: WikiSearchMode; // 'docs' = one best hit per document
  expand?: number; // 0-2 neighbor expansion; citations widen accordingly
}

export interface WikiCitation {
  uri: string; // wiki://{space}/{doc_id}@{version_id}#{start}-{end}
  doc_id: number;
  version_id: number;
  chunk_id: number;
  heading_path: string[];
  anchor: string; // stable section anchor for wiki_read
  byte_range: [number, number];
  checksum: string; // verifiable via /wiki/verify
  quote: string;
}

export interface WikiHit {
  text: string;
  doc_title: string;
  heading_path: string[];
  citation: WikiCitation;
}

export interface WikiSearchOutput {
  hits: WikiHit[];
  total_docs_matched: number;
}

export type WikiSelector =
  | { type: 'toc' }
  | { type: 'section'; anchor: string }
  | { type: 'range'; start: number; end: number }
  | { type: 'full' };

export interface WikiReadInput {
  doc_id: number;
  version?: number; // time-travel read of a historical version
  selector?: WikiSelector; // default { type: 'full' }
}

export interface WikiTocEntry {
  anchor: string;
  heading_path: string[];
  byte_start: number;
  byte_end: number;
}

export interface WikiReadOutput {
  doc_id: number;
  version_id: number;
  is_current: boolean;
  title: string;
  status: WikiDocStatus;
  checksum: string;
  size: number;
  toc?: WikiTocEntry[]; // for the 'toc' selector
  content?: string; // for section/range/full selectors
  byte_range?: [number, number];
  truncated: boolean; // full reads are bounded (256 KiB)
}

export interface WikiVerifyInput {
  uri?: string; // wiki:// citation URI, or pass the explicit fields below
  doc_id?: number;
  version_id?: number;
  byte_range?: [number, number];
  checksum?: string; // compared against the recomputed checksum when present
}

export type WikiVerifyStatus = 'valid' | 'superseded' | 'invalid' | 'not_found';

export interface WikiVerifyOutput {
  status: WikiVerifyStatus; // 'superseded' = intact but a newer version exists
  current_version?: number;
  checksum?: string; // recomputed from immutable content
  quote?: string;
}

export interface WikiBundleEntry {
  path: string; // bundle-relative path, e.g. "guides/setup.md"
  content: string;
}

export interface WikiImportInput {
  entries: WikiBundleEntry[]; // OKF v0.1 bundle files (Markdown + YAML frontmatter)
  namespace?: string; // default "default"; bundles round-trip per namespace
}

export type WikiImportStatus = 'created' | 'updated' | 'unchanged';

export interface WikiImportOutput {
  created: number;
  updated: number;
  unchanged: number; // checksum-idempotent: re-imports never grow versions
  docs: { path: string; doc_id: number; version_id: number; status: WikiImportStatus }[];
  skipped?: { path: string; reason: string }[];
}

export interface WikiExportOutput {
  namespace: string;
  entries: WikiBundleEntry[]; // concept .md files + index.md + manifest.json
  docs: number;
}

export interface WikiEventInfo {
  id: number;
  // DocCreated | VersionCommitted | DocArchived | DocRestored | OrphanSwept
  // | CitationVerifyFailed | ImportCompleted | ExportCompleted
  // | DigestExtracted | WikiQueried | WikiRead | StaleReport | EventsPruned
  kind: string;
  doc_id?: number;
  version_id?: number;
  actor: string;
  detail?: Record<string, unknown>;
  created_at: number;
}

export interface WikiDigestReport {
  digested: number; // versions distilled into the Cognitive Nexus
  facts: number; // facts this document currently claims
  superseded: number; // claims the digest retracted because the document dropped them
  skipped: number;
  citations_checked: number; // post-run citation sample
  citations_invalid: number;
  usage: Usage;
}

export interface McpServerConfig {
  space_id: string;
  auth_token?: string;
  auto_create_space?: boolean;
  auto_create_tier?: number;
}

export interface McpHttpServerConfig {
  path_prefix?: string; // default "/mcp"; clients connect to {path_prefix}/{space_id}
  allowed_hosts?: string[]; // default loopback-only in rmcp; set company domains explicitly
  allowed_origins?: string[]; // for browser-based MCP clients
  auto_create_space?: boolean;
  auto_create_tier?: number;
}

export interface Concept {
  id: string; // engine-assigned element id, e.g. "C-7"
  kind: 'concept';
  space_id?: string;
  schema_ref?: string; // the exact type symbol, e.g. "kip://profiles/cognitive-memory@2.1.0/Person"
  key?: string; // immutable Space-local logical key — the caller's handle
  name?: string; // mutable display label; never identity
  canonical_id?: string;
  aliases?: string[];
  attributes?: Record<string, unknown>;
  facets?: Record<string, Record<string, unknown>>; // e.g. MnemonicState
  retention?: { retention_class?: string; expires_at?: string; legal_hold?: boolean };
  _system?: Record<string, unknown>; // engine truth: version, created_at, state, origin
}

export interface ModelConfig {
  family: string; // "gemini", "anthropic", "openai", "deepseek", "mimo" etc.
  model: string;
  api_base: string;
  api_key: string;
  disabled?: boolean;
  label?: string;
  effort?: 'minimal' | 'low' | 'medium' | 'high' | 'max';
  bearer_auth?: boolean;
  stream?: boolean;
  context_window?: number;
  max_output?: number;
}

export interface SpaceTier {
  tier: number;
  updated_at: number; // Unix timestamp in milliseconds
}

export interface SpaceToken {
  token: string; // full value only in the add_space_token response; redacted to a prefix elsewhere
  name: string; // required, unique per space; audit identity and revocation handle
  scope: TokenScope;
  usage: number;
  created_at: number; // Unix timestamp in milliseconds
  updated_at: number; // Unix timestamp in milliseconds
  expires_at?: number; // Unix timestamp in milliseconds
  labels?: string[]; // wiki ACL labels: [] sees unlabeled content only; omitted = unrestricted
}

export interface StorageStats {
  [k: string]: number | string | boolean | null;
}

export interface SpaceInfo {
  id: string;
  name?: string;
  description?: string;
  owner: string;
  db_stats: StorageStats;
  concepts: number;
  propositions: number;
  conversations: number;
  public: boolean;
  tier: SpaceTier;
  formation_usage: Usage;
  recall_usage: Usage;
  maintenance_usage: Usage;
  formation_processed_id: number;
  maintenance_processed_id: number;
  maintenance_at: MaintenanceAt;
  wiki_docs: number;
  wiki_chunks: number;
  wiki_versions: number;
  wiki_queries: number;
  wiki_digested: number; // digest high-water mark (version id)
  wiki_stale_docs: number; // from the last housekeeping stale scan
}

export interface FormationStatus {
  id: string;
  concepts: number;
  propositions: number;
  conversations: number;
  formation_processing: boolean;
  maintenance_processing: boolean;
  formation_processed_id: number;
  maintenance_processed_id: number;
  maintenance_at: MaintenanceAt;
}

export interface MaintenanceAt {
  daydream: number;
  full: number;
  quick: number;
  /** Start time of the latest maintenance task in unix milliseconds, 0 if none started. */
  start_at: number;
}

export interface Usage {
  /** Input tokens sent to the LLM. */
  input_tokens: number;
  /** Output tokens received from the LLM. */
  output_tokens: number;
  /** Cached tokens used in the execution. */
  cached_tokens: number;
  /** Number of requests made to models, agents, or tools. */
  requests: number;
}

export interface AgentOutput {
  content: string;
  conversation?: number;
  failed_reason?: string;
  usage?: Usage;
  model?: string;
  [k: string]: unknown;
}

export type ConversationStatus =
  | 'submitted'
  | 'working'
  | 'idle'
  | 'completed'
  | 'failed'
  | 'cancelled';

export interface Conversation {
  _id: number;
  user: string;
  thread?: string;
  label?: string;
  messages: Message[];
  resources: unknown[];
  artifacts: unknown[];
  status: ConversationStatus;
  failed_reason?: string | null;
  period: number;
  created_at: number;
  updated_at: number;
  usage: Usage;
  steering_messages?: string[];
  follow_up_messages?: string[];
  ancestors?: number[];
}

export interface ConversationDelta {
  _id: number;
  messages: unknown[];
  artifacts: unknown[];
  status: ConversationStatus;
  usage: Usage;
  failed_reason?: string | null;
  updated_at: number;
  child?: number | null;
}

export interface ServiceInfo {
  name: string;
  version: string;
  sharding: number;
  description: string;
}

export type KipOperation = string | {
  op_id?: string;
  language?: 'KQL' | 'KML' | 'META'; // advisory; parsed command controls the read-only gate
  command?: string;
  ast?: unknown;
  parameters?: Record<string, unknown>;
  idempotency_key?: string;
  options?: { extensions?: Record<string, unknown> };
  extensions?: Record<string, unknown>;
};

export interface KipRequest {
  command?: string; // a single command; mutually exclusive with `operations`
  operations?: KipOperation[]; // several commands in one round-trip
  execution?: { mode: 'independent' | 'sequence' | 'atomic'; on_error?: 'stop' | 'continue'; isolation?: string; idempotency_key?: string; extensions?: Record<string, unknown> }; // required for more than one operation
  read?: { snapshot_token?: string; extensions?: Record<string, unknown> }; // bind every operation to one read coordinate
  parameters?: Record<string, unknown>; // values bound into `:placeholders`
  dry_run?: boolean; // validate and plan without committing
}

export interface KipError {
  code: string; // a registry name, e.g. "NotFoundOrNotVisible" — not a number
  message: string;
  category?: string;
  hint?: string;
  retry?: { class: string; after_ms?: number };
  details?: unknown;
}

export interface KipOperationResult<T> {
  op_id?: string;
  status: 'succeeded' | 'failed' | 'skipped' | 'rolled_back' | 'no_effect';
  result?: T;
  context?: unknown;
  error?: KipError;
  warnings?: unknown[];
  next_cursor?: string;
  receipt?: unknown;
  extensions?: Record<string, unknown>;
}

export interface KipResponse<T> {
  kip: '2.0';
  request_id?: string;
  status: 'succeeded' | 'failed' | 'partial' | 'outcome_unknown';
  results: KipOperationResult<T>[];
  execution?: unknown;
  context?: unknown;
  snapshot?: { space_id?: string; snapshot_seq: number; schema_environment_version?: number; snapshot_token?: string; extensions?: Record<string, unknown> };
  receipt?: unknown;
  warnings?: unknown[];
  next_cursor?: string;
  error?: KipError; // set only when the request failed before its operations
  extensions?: Record<string, unknown>;
}
```

> **KIP 2.0.** A failure lives at the operation level for an ordinary error and
> at the request level only for an envelope error, so a client must read both.
> `outcome_unknown` is neither success nor failure: a write may have committed,
> and the recovery is to look the transaction up — never to re-send it as if
> nothing had happened.

---

## 3) MCP Server

By default, the HTTP service exposes a Streamable HTTP MCP endpoint for MCP-capable agents; `MCP_HTTP_ENABLED=false` disables it:

```text
https://your-brain-host/mcp/my_space_001
```

Clients select the target memory space from the URL path and pass the same CWT or space token used by REST as `Authorization: Bearer <token>`. This is the recommended mode for internal multi-user agent platforms where each employee receives a dedicated Brain space.

Anda Brain can also run as a local MCP stdio server:

```bash
MCP_AUTH_TOKEN="$SPACE_TOKEN" \
  anda_brain mcp --space-id my_space_001 local --db ./data
```

Both MCP modes use the same model, auth, and storage configuration as the HTTP service. For stdio, the nested storage subcommand is optional; omit it for in-memory development, use `local --db ./data` for local persistence, or use `aws --bucket ... --region ...` for S3.

| Tool | Input | Output | Scope |
| ---- | ----- | ------ | ----- |
| `anda_brain_remember_conversation` | `FormationInput` shape (`messages`, `context`, `timestamp`) | `AgentOutput` | `write` |
| `anda_brain_recall_memory` | `RecallInput` shape (`query`, `context`, optional `budget`) | `AgentOutput` | `read` |
| `anda_brain_run_maintenance` | `MaintenanceInput` shape | `AgentOutput` | `write` |
| `anda_brain_get_space_info` | none | `SpaceInfo` | `read` |
| `anda_brain_get_formation_status` | none | `FormationStatus` | `read` |
| `anda_brain_execute_kip_readonly` | `{ command?, commands?, parameters?, dry_run? }` | `KipResponse` | `read` |
| `anda_brain_get_or_init_user` | `{ user, name? }` | `Concept` | `write` |
| `anda_brain_list_conversations` | `{ collection?, cursor?, limit? }` | `{ conversations, next_cursor }` | `read` |
| `anda_brain_get_conversation` | `{ conversation_id, collection?, delta?, messages_offset?, artifacts_offset? }` | `Conversation` or `ConversationDelta` | `read` |
| `anda_brain_wiki_search` | wiki query, filters, and result limit | `WikiSearchOutput` | `read` |
| `anda_brain_wiki_read` | document id and selector | `WikiReadOutput` | `read` |
| `anda_brain_wiki_commit` | document fields and full Markdown content | `WikiCommitOutput` | `write` |
| `anda_brain_wiki_verify` | citation URI or explicit citation fields | `WikiVerifyOutput` | `read` |

The MCP read-only KIP tool uses `commands` and supplies independent execution for batches. The HTTP `/execute_kip_readonly` endpoint uses `operations` and requires an explicit `execution.mode` for batches. Wiki MCP tools are available when the `wiki` feature is enabled (always true for the service binary).

When `ED25519_PUBKEYS` is set, configure the remote MCP client with an `Authorization` bearer token, or configure stdio with `MCP_AUTH_TOKEN` / `--mcp-auth-token`. `read` tools can also access public spaces without a token. For remote MCP behind a company domain or reverse proxy, set `MCP_HTTP_ALLOWED_HOSTS` to the accepted Host values. Use `--mcp-auto-create-space` for local stdio development or `MCP_HTTP_AUTO_CREATE_SPACE=true` for remote development if the target space does not exist yet; remote auto-create requires `ED25519_PUBKEYS` plus a CWT with `write` scope for the target space before the missing space is created.

---

## 4) Endpoint List

## 4.1 Public Endpoints

### GET `/favicon.ico` and GET `/apple-touch-icon.webp`

- Description: Static product icons
- Auth: None
- Response: `image/x-icon` or `image/webp`

### GET `/info`

- Description: Service information
- Auth: None
- Response (JSON): `ServiceInfo`

### GET `/SKILL.md`

- Description: Returns the skill description in Markdown
- Auth: None
- Response: `text/markdown`

---

## 4.2 Space Business Endpoints (`/v1/{space_id}`)

### POST `/v1/{space_id}/formation`

- Purpose: Submit a memory formation task
- Auth: SpaceToken/CWT `write`
- Request body: `FormationInput` (raw string is also accepted in Markdown mode)
- Observation time accepts RFC 3339 offsets and fractional precision, normalized to `YYYY-MM-DDTHH:mm:ss.SSSZ`. An invalid or missing timestamp uses the stored conversation creation time; retries use the same fallback. The original input is retained. Markdown text is captured verbatim as one user message with the same `:msg1` Evidence binding.
- Response (JSON/CBOR): `RpcResponse<AgentOutput>`
- Response (Markdown): `string` (returns only `AgentOutput.content`)

### POST `/v1/{space_id}/recall`

- Purpose: Recall memory via natural-language query
- Auth: SpaceToken/CWT `read` (public spaces are unauthenticated; private spaces require a valid token). Label-restricted space tokens receive `403` because agentic Recall can traverse all wiki labels.
- Request body: `RecallInput` (raw string is also accepted in Markdown mode)
- Response (JSON/CBOR): `RpcResponse<AgentOutput>`
- Response (Markdown): plain `AgentOutput.content`

### POST `/v1/{space_id}/recall_structured`

- Purpose: Return a synthesized answer with trace-derived memory citations, `found`, and optional uncertainty.
- Auth and request body: same as `/recall`; label-restricted tokens receive `403`.
- Response (JSON/CBOR): `RpcResponse<RecallOutput>`
- Response (Markdown): plain `RecallOutput.answer`

<a id="recall-budget-contract"></a>

### Recall budget contract

Optional `budget` enables a host-selected JSON memory packet in `content` rather
than a free-form answer. `memory_policy.recall_budget` can enforce the same
limits for every Recall; a request may tighten them but cannot raise or disable
the policy. An absent/null policy and request preserve the legacy response.

The fixed codec counts the entire compact packet, including escaping and
coverage. `context_tokens` additionally bounds the cumulative versioned
serialization of planner input across this Recall. Provider message templates,
billing and RPC/MCP transport replicas are outside these scopes. No model-name
tokenizer guessing or heuristic fallback is used. Diagnostic histories, thoughts,
artifacts and tool calls are not returned alongside the budgeted packet.

`recall_structured` puts this same packet in `answer`, leaves trace citations
out of the envelope, and adds `memory_budget` with `tokenizer`, `token_limit`,
`tokens`, and `context_token_limit`; `found` means non-primer candidates were
delivered, not that semantic relevance or completeness was proved. Markdown
returns the same packet text. `budget_insufficient` or literal `null` with a
static `failed_reason` is unusable/incomplete, not a successful empty answer.
Budget-mode failures use fixed codes: `recall_output_budget_exhausted`,
`recall_required_read_incomplete`, `recall_context_budget_exhausted`,
`recall_deadline_reached`, `recall_model_unavailable`,
`recall_planner_incomplete`, or `recall_procedure_window_incomplete`.
A packet is always a candidate read (`semantic_complete=false`, `action_ready=false`).
Required constraints and warnings are an indivisible set; if they do not fit,
ordinary memories are withheld. No fallback token estimate or free-form answer
bypasses this contract.

### POST `/v1/{space_id}/maintenance`

- Purpose: Trigger maintenance (sleep/consolidation)
- Auth: SpaceToken/CWT `write`
- Request body: `MaintenanceInput`
- Response: `RpcResponse<AgentOutput>`

Explicit `parameters` override the space policy; omitted members use policy defaults. The same effective parameters reach deterministic settlement and the model, without changing the persisted space policy.

### POST `/v1/{space_id}/memory/pin`

- Purpose: Pin or unpin one graph entity to exempt its memory strength from disuse decay.
- Auth: SpaceToken/CWT `write`
- Request body: `MemoryPinInput`; `pinned` defaults to `true`.
- Response: `RpcResponse<MemoryPinOutput>`

### POST `/v1/{space_id}/memory/forget`

- Purpose: Physically remove graph entities; inspect `dry_run: true` before deletion.
- Auth: SpaceToken/CWT `write`
- Request body: `MemoryForgetInput`; `dry_run` defaults to `false`.
- Response: `RpcResponse<MemoryForgetReport>`; per-entity errors appear in `result.entities`.

Accepts explicit Concept (`C-*`), Proposition (`P-*`), Assertion (`A-*`), Evidence (`E-*`) and Activity (`X-*`) IDs, including the Evidence containing captured message text. Native legal holds and reference checks still apply; successful purges leave erased identity stubs. Counts report each erased kind, including cascades. This removes the selected graph records; stored conversations, wiki documents and external copies have separate lifecycles.

### GET `/v1/{space_id}/memory_status`

- Purpose: Read memory statistics and the latest maintenance report.
- Auth: SpaceToken/CWT `read`; public spaces permit anonymous reads.
- Response: `RpcResponse<MemoryStatus>`, with existing JSON/CBOR/Markdown negotiation.
- `result.last_settlement.correction_scan_incomplete`: the bounded scan has not proved the backlog exhausted; later maintenance resumes within the transaction.
- `result.last_settlement.correction_scan_through_seq`: largest transaction sequence completely read.
- `result.last_settlement.correction_scan_error`: discovery failure; failed scans retain their cursor.
- These fields describe correction discovery, not completed model review or authorized Watch coverage.

### POST `/v1/{space_id}/execute_kip_readonly`

- Purpose: Execute a KIP 2.0 request (read-only: KQL and META)
- Auth: SpaceToken/CWT `read` (public spaces are unauthenticated; private spaces require a valid token)
- Request body: `KipRequest`, or a bare JSON string read as one command
- A batch with more than one `operations` entry must declare `execution.mode`; check `status` and each `results[].status` even when HTTP returns `200`.
- Response: `KipResponse<T>` (the result type follows the commands)
- Read-only is enforced on what each command *parses to*, so a KML mutation is refused here however the request labels it

### POST `/v1/{space_id}/get_or_init_user`

- Purpose: Get or initialize a user concept node for the given principal
- Auth: SpaceToken/CWT `write`
- Request body: `GetOrInitUserInput`
- Omitting `name` preserves an existing display name. An explicit `name` updates it; new unnamed users use their key as the initial display name.
- Response: `RpcResponse<Concept>`

### GET `/v1/{space_id}/info`

- Purpose: Get space status and statistics
- Auth: SpaceToken/CWT `read` (public spaces are unauthenticated; private spaces require a valid token)
- Response: `RpcResponse<SpaceInfo>`

### GET `/v1/{space_id}/status`

- Alias for `/v1/{space_id}/info`, with the same authorization and response.

### POST `/v1/{space_id}/probe`

- Purpose: model-free retrieval reachability; `found:false` is not a rejected belief.
- Auth: the existing Space `read` permission and public-space read rules.
- Request: `{"query":"...", "limit":8}`.
- Response: `RpcResponse<ProbeOutput>` with `found`, `negative_cached`, optional `hits` and optional `search_exhaustive`. Coverage comes from the SEARCH result's top-level field; omission means unknown and remaining pagination means false. The negative cache stores only explicitly exhaustive search misses, not proof of absence or rejected belief.

### GET `/v1/{space_id}/formation_status`

- Purpose: Get formation status
- Auth: SpaceToken/CWT `read` (public spaces are unauthenticated; private spaces require a valid token)
- Response: `RpcResponse<FormationStatus>`
- This is lightweight monitoring; cursors are not per-job success evidence. Rust hosts can use `Space::processing_report` / `wait_for_processing` to distinguish queued, running, failed, cancelled, interrupted and timed-out work. Reconcile the same conversation ID after timeout instead of resubmitting.

### GET `/v1/{space_id}/conversations/{conversation_id}?collection=<collection>`

- Purpose: Get a single conversation detail
- Auth: SpaceToken/CWT `read` (public spaces are unauthenticated; private spaces require a valid token); tokens restricted by ACL labels are rejected with `403` — conversations persist the full agent runner history, which is not label-scoped; `collection=recall` additionally rejects anonymous access on public spaces with `403` (recall runs from a private era may embed labeled wiki content)
- Query:
  - `collection?: string` // "formation" (default), "recall" or "maintenance"; unknown values are rejected with `400`
- Response: `RpcResponse<Conversation>`

### GET `/v1/{space_id}/conversations/{conversation_id}/delta?collection=<collection>&messages_offset=<n>&artifacts_offset=<n>`

- Purpose: Get incremental conversation updates after client-side offsets
- Auth: SpaceToken/CWT `read` (public spaces are unauthenticated; private spaces require a valid token); tokens restricted by ACL labels are rejected with `403` (`collection=recall`: anonymous access on public spaces is also rejected)
- Query:
  - `collection?: string` // "formation" (default), "recall" or "maintenance"; unknown values are rejected with `400`
  - `messages_offset?: number` // returns only messages after this offset, defaults to `0`
  - `artifacts_offset?: number` // returns only artifacts after this offset, defaults to `0`
- Response: `RpcResponse<ConversationDelta>`

### GET `/v1/{space_id}/conversations?collection=<collection>&cursor=<cursor>&limit=<n>`

- Purpose: List conversations with pagination
- Auth: SpaceToken/CWT `read` (public spaces are unauthenticated; private spaces require a valid token); tokens restricted by ACL labels are rejected with `403` (`collection=recall`: anonymous access on public spaces is also rejected)
- Query:
  - `collection?: string` // "formation" (default), "recall" or "maintenance"; unknown values are rejected with `400`
  - `cursor?: string`
  - `limit?: number`
- Response: `RpcResponse<Conversation[]>` (next page cursor is returned via `next_cursor`)

---

## 4.3 Wiki Endpoints (`/v1/{space_id}/wiki`)

The wiki is the space's versioned reference memory (policies, manuals, SOPs, API docs). Writes are git-like immutable commits with CAS concurrency control; searches return verifiable `wiki://` citations. ACL: documents may carry an `acl_label`; space tokens with `labels` see unlabeled content plus their granted labels — enforced inside the retrieval query itself. Anonymous readers of public spaces see unlabeled content only; denials surface as 404.

Wiki-specific error semantics: `409` commit conflict (`RpcError.data.current_version` carries the version to rebase on), `413` content over 1 MiB, `404` not found / ACL-denied.

### POST `/v1/{space_id}/wiki/docs`

- Purpose: Commit a document (create, or CAS update with `doc_id` + `parent_version`); identical content is a no-op
- Auth: SpaceToken/CWT `write`
- Request body: `WikiCommitInput` (raw Markdown string is also accepted; the title derives from the first heading)
- Response: `RpcResponse<WikiCommitOutput>`

### GET `/v1/{space_id}/wiki/docs?namespace=<ns>&status=<status>&tag=<tag>&cursor=<cursor>&limit=<n>`

- Purpose: List documents with pagination
- Auth: SpaceToken/CWT `read` (public spaces are unauthenticated; ACL labels apply)
- Response: `RpcResponse<WikiDocInfo[]>` (next page cursor via `next_cursor`)

### GET `/v1/{space_id}/wiki/docs/{doc_id}`

- Purpose: Document metadata plus table of contents
- Auth: SpaceToken/CWT `read` (public spaces are unauthenticated; ACL labels apply)
- Response: `RpcResponse<{ doc: WikiDocInfo; toc: WikiTocEntry[] }>`

### GET `/v1/{space_id}/wiki/docs/{doc_id}/content?version=<id>&anchor=<anchor>&start=<n>&end=<n>`

- Purpose: Progressive reading — `anchor` reads one section, `start`+`end` a byte range, neither reads the bounded full text; `version` time-travels
- Auth: SpaceToken/CWT `read` (public spaces are unauthenticated; ACL labels apply)
- Response: `RpcResponse<WikiReadOutput>`

### GET `/v1/{space_id}/wiki/docs/{doc_id}/versions?cursor=<cursor>&limit=<n>`

- Purpose: Version history (immutable commit chain)
- Auth: SpaceToken/CWT `read` (public spaces are unauthenticated; ACL labels apply)
- Response: `RpcResponse<WikiVersionInfo[]>` (next page cursor via `next_cursor`)

### POST `/v1/{space_id}/wiki/docs/{doc_id}/archive`

- Purpose: Archive a document (hidden from search, still readable by id, restorable)
- Auth: SpaceToken/CWT `write`
- Response: `RpcResponse<WikiDocInfo>`

### POST `/v1/{space_id}/wiki/docs/{doc_id}/restore`

- Purpose: Restore an archived document into search
- Auth: SpaceToken/CWT `write`
- Response: `RpcResponse<WikiDocInfo>`

### POST `/v1/{space_id}/wiki/search`

- Purpose: BM25 keyword retrieval over document passages, returning snippets with verifiable citations
- Auth: SpaceToken/CWT `read` (public spaces are unauthenticated; ACL labels apply)
- Request body: `WikiSearchInput` (raw query string is also accepted)
- Response: `RpcResponse<WikiSearchOutput>`

### POST `/v1/{space_id}/wiki/verify`

- Purpose: Verify a citation against immutable stored content
- Auth: SpaceToken/CWT `read` (public spaces are unauthenticated; ACL labels apply)
- Request body: `WikiVerifyInput` (raw `wiki://` URI string is also accepted)
- Response: `RpcResponse<WikiVerifyOutput>`

### GET `/v1/{space_id}/wiki/events?kind=<kind>&doc_id=<id>&cursor=<cursor>&limit=<n>`

- Purpose: Query the append-only audit log (writes, imports, digests; reads too when `wiki_audit_reads` is enabled)
- Auth: SpaceToken/CWT `read`; tokens restricted by ACL labels are rejected with `403`
- Response: `RpcResponse<WikiEventInfo[]>` (next page cursor via `next_cursor`)

### POST `/v1/{space_id}/wiki/import`

- Purpose: Import an OKF v0.1 bundle; checksum-idempotent (re-imports never grow version chains); unknown frontmatter keys survive round-trips verbatim
- Auth: SpaceToken/CWT `*` (full scope)
- Request body: `WikiImportInput`
- Response: `RpcResponse<WikiImportOutput>`

### GET `/v1/{space_id}/wiki/export?namespace=<ns>`

- Purpose: Export one namespace as an OKF bundle (concept `.md` files + `index.md` + `manifest.json` with checksums); replayable into an empty space
- Auth: SpaceToken/CWT `*` (full scope)
- Response: `RpcResponse<WikiExportOutput>`

### POST `/v1/{space_id}/wiki/digest`

- Purpose: Distill pending wiki versions into the Cognitive Nexus — each fact becomes a Proposition plus an Assertion attributed to the Brain, citing its passage as Evidence. A fact the newest version no longer states has the digest's own Assertion retracted; the Proposition and anyone else's Assertions about it are untouched. Requires `update_space {"wiki_digest": true}`
- Auth: SpaceToken/CWT `write`
- Response: `RpcResponse<WikiDigestReport>`

---

## 4.4 Space Management Endpoints (`/v1/{space_id}/management`)

### GET `/v1/{space_id}/management/space_tokens`

- Purpose: List Space Tokens
- Auth: Must pass CWT `write` (user management-level auth)
- Response: `RpcResponse<SpaceToken[]>` — the `token` field is redacted to a display prefix (e.g. `STabc123…`); full token values are only returned once, by `add_space_token`. Save them at mint time, or revoke by `name`.

### POST `/v1/{space_id}/management/add_space_token`

- Purpose: Add a Space Token
- Auth: Must pass CWT `write` (user management-level auth). Minting a `*` (full-scope) token requires a `*`-scoped CWT — a `write` CWT cannot mint tokens above its own scope.
- Request body: `AddSpaceTokenInput` — `name` is required and unique within the space. `labels` is allowed only for `read` tokens; `[]` restricts reads to unlabeled wiki content, while omission is unrestricted.
- Response: `RpcResponse<SpaceToken>` (new token, always prefixed with `ST`; this is the only response that carries the full token value)

### POST `/v1/{space_id}/management/revoke_space_token`

- Purpose: Revoke a Space Token
- Auth: Must pass CWT `write` (user management-level auth)
- Request body: `RevokeSpaceTokenInput` — either `token` (the full token value) or `name` (the unique token name, for managers who did not save the value at mint time)
- Response: `RpcResponse<boolean>` (whether revocation succeeded)

### PATCH `/v1/{space_id}/management/update_space`

- Purpose: Update space information, wiki settings, and the optional `memory_policy` (validated and persisted as a replacement policy).
- Auth: Must pass CWT `write` (user management-level auth)
- Request body: `UpdateSpaceInput`
- Response: `RpcResponse<true>`

### POST `/v1/{space_id}/management/shadow_eval`

- Purpose: Compare a candidate memory policy against the current one by replaying recent Recall queries on forks. This can make multiple model calls.
- Auth: CWT `write` (space tokens are not accepted).
- Request body: `ShadowEvalInput`; `replay_sample` defaults to the space policy and is capped at `16`.
- Response: `RpcResponse<ShadowReport>`

### PATCH `/v1/{space_id}/management/restart_formation`
- Purpose: Restart a formation task by conversation ID (for failed/stale formations)
- Auth: Must pass CWT `write` (user management-level auth)
- Request body: `FormationRestartInput`
- Response: `RpcResponse<true>`

### GET `/v1/{space_id}/management/space_byok`
- Purpose: Get BYOK (Bring Your Own Key) configuration, i.e., use custom model configuration
- Auth: Must pass CWT `write` (user management-level auth; response includes provider credentials)
- Response: `RpcResponse<ModelConfig>`

### PATCH `/v1/{space_id}/management/space_byok`
- Purpose: Update BYOK (Bring Your Own Key) configuration, i.e., use custom model configuration
- Auth: Must pass CWT `write` (user management-level auth)
- Request body: `ModelConfig`
- Response: `RpcResponse<true>`

---

## 4.5 Admin Endpoints (`/admin`)

### POST `/admin/create_space`

- Purpose: Create a space
- Auth: Platform admin + CWT `write`
- Request body: `CreateOrUpdateSpaceInput`
- Response: `RpcResponse<SpaceInfo>`

### POST `/admin/{space_id}/update_space_tier`

- Purpose: Update space tier
- Auth: Platform admin + CWT `write`
- Request body: `CreateOrUpdateSpaceInput`
- Response: `RpcResponse<SpaceTier>`

---

## 5) Frontend Call Example (TS)

```ts
async function rpcPost<TReq, TRes>(
  url: string,
  body: TReq,
  token?: string
): Promise<RpcResponse<TRes>> {
  const res = await fetch(url, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      Accept: 'application/json',
      ...(token ? { Authorization: `Bearer ${token}` } : {}),
    },
    body: JSON.stringify(body),
  });

  const responseText = await res.text();
  if (!res.ok) {
    let message = responseText || `HTTP ${res.status}`;
    try {
      const error = JSON.parse(responseText) as RpcError;
      if (error.message) message = error.message;
    } catch { /* middleware errors can be plain text */ }
    throw new Error(`HTTP ${res.status}: ${message}`);
  }
  return JSON.parse(responseText) as RpcResponse<TRes>;
}

// Recall
const recall = await rpcPost<RecallInput, AgentOutput>(
  '/v1/my_space_001/recall',
  { query: 'What are this user\'s preferences?', context: { counterparty: 'user_1' } },
  'YOUR_TOKEN'
);

if (recall.error) {
  console.error(recall.error.message);
} else {
  console.log(recall.result?.content);
}
```

---

## 6) Error Semantics

- Authentication failure: HTTP `401`, response body is `RpcError`
- Invalid request/parameters: HTTP `400`, response body is `RpcError`
- Forbidden access: HTTP `403`; missing space or wiki document: HTTP `404`
- Wiki commit conflicts: HTTP `409`, with the current version in `RpcError.data.current_version`; oversized wiki content: HTTP `413`
- LLM request load shedding: HTTP `429`; global HTTP load shedding: HTTP `503`. These middleware responses are plain text.
- Success: HTTP `200`, response body is usually `RpcResponse<T>`
- Handler errors are JSON even when `Accept` requests CBOR or Markdown. Unmatched routes and middleware errors may have a plain text or empty body. Only success bodies follow `Accept`.
- A KIP request can return HTTP `200` with `KipResponse.status` or an operation's `status` equal to `failed`; inspect both KIP levels.
- MCP tools mirror this classification: caller-fixable failures surface as JSON-RPC `invalid_params`/`invalid_request` (a wiki commit conflict carries the same `data.current_version` retry payload as the HTTP `409` body), and only true internal failures use `internal_error`


### Isolated MIB host

The sibling Anda Bot `mib` feature exposes separate loopback protocols at
`/mib-agent/v0.1` and `/mib-memory/v0.1`. These are not routes of this production
Brain API. They use `experiments` for isolated state, completion barriers,
monotonic business time and cleanup. See [integration](README.md#mib-integration).
The adapter does not advertise online learning; costs with missing provider or
observer telemetry remain incomplete.

The Rust `Experiment::create_with_recall_budget` factory persists a forced Recall
budget before exposing a run. `audit_procedures()` returns a bounded read-only
native inventory; truncated counts cannot prove absence. The Bot-only MIB
extension `learning_audit` exposes this inventory to the evaluator, never the
business model. It neither enables learning nor grants execution authority.
See [validation](README.md#mib-integration).

The former Rust `anda_brain::eval` API and `eval` CLI (including optimizer/miner
flags) have been retired. MIB supplies the public product-regression
profile; self-test, shadow diagnostics, probes, citations and ledgers remain
online instruments. Runtime policies use the Space's persisted `MemoryPolicy`.
Trusted Rust hosts can call `AppState::with_agent_prompts(AgentPrompts)` before
sharing/opening the host to supply immutable deployment sections; each section
starts with `# A.`, fits 128 KiB and retains the compiled reference prefix.
This configuration is not an HTTP/MCP prompt operation. See [migration](README.md#offline-regression-and-instance-configuration).


### Trusted learning runtime (Rust only)

With `learning`, `Space::learning()` exposes explicit registration, frozen
cohorts, bounded `drive` steps, metadata discovery after restart, and separately
authenticated Outcome ingestion. Native leases, current executable authority,
dependency validity and policy pins gate dispatch. These are host APIs and add
no HTTP/MCP routes or model mutation authority. Cohort completion does not adopt
a Skill. `settle(job_id)` recomputes the native ledger after the fixed cutoff
and atomically records the verdict/standing. `reviews()` and `enroll_review()`
provide recoverable monitoring with retained acquisition evidence;
`submit_safety_signal()` accepts separately authenticated safety withdrawal.
`bind_application_context()` accepts short-lived trusted host observations;
`procedure_status()` and Recall's internal `check_procedure_status` tool read
current eligibility without granting execution permission. Unverified conditions
or an expired review block recommendations. Configured learning Spaces cannot
copy their operational journals via fork/snapshot. See [runtime](README.md#native-learning-contracts)
and [lifecycle and recovery](README.md#native-learning-contracts).


## Authenticated runtime inbox and observations

Configure `BRAIN_RUNTIME_CONFIG` before startup. See [runtime setup, contracts,
recovery and examples](RUNTIME.md) and [runtime.example.json](runtime.example.json).
These endpoints require real credentials even on public/local Spaces.

| Endpoint | Required access | Result |
| --- | --- | --- |
| `GET /v1/{space_id}/attention?limit=20&cursor=...` | Verified read/* credential, explicit native identity and audience | `AttentionPage`; reading never claims work |
| `POST /v1/{space_id}/attention/{id}/responses` | Verified write/*, current visibility and correct recipient | `ResponseReceipt`; body `AttentionResponse` |
| `POST /v1/{space_id}/outcomes` | Signed observer-mapped CWT + current `record_outcome` + registered contract | `ObservationReceipt`; body `OutcomeInput` |
| `GET /v1/{space_id}/runtime/status` | Verified read/*, explicit mapping when configured | `RuntimeStatus`; counts cover only the visible bounded page |

The URL `id` is the returned wake hash, not its slash-containing `wake_ref`.
Cursor tokens expire after five minutes and are authenticated/encrypted for the
caller/instance/configuration. Cross-caller or stale tokens are rejected. Public
Spaces and ordinary write tokens do not authenticate independent observers. MCP
exposes `anda_brain_get_attention`, `anda_brain_respond_attention` and
`anda_brain_get_runtime_status`; observer writes are not model tools.

POST bodies are structured JSON/CBOR; responses retain JSON/CBOR/Markdown
negotiation. Same event/body is idempotent; changed content returns 409 with retained
audit. Future or wrong-instance input is rejected. Late/correction/safety receipt
status, native persistence and learning eligibility remain separate. Registered
learning measurements must match the existing `OutcomeMeasurements` contract and
are routed exclusively to its controller, including baseline Attempts.

```ts
type RuntimeScope = { space_id: string; space_instance: string };
type AttentionQuery = { cursor?: string | null; limit?: number | null };
type AttentionResponse =
  | { kind: "clarification"; event_key: string; answer: string }
  | { kind: "agent_statement"; event_key: string; statement: string };
type ResponseReceipt = {
  receipt_id: string; status: string; evidence_ref: string | null;
};
type AttentionPage = {
  scope: RuntimeScope; items: AttentionItem[];
  next_cursor: string | null; complete: boolean;
};
type AttentionItem = {
  id: string; wake_ref: string; parent_id: string | null;
  watch_ref: string; fire_activity_ref: string; summary: string;
  state: "pending" | "running" | "blocked" | "completed" | "cancelled";
  reason: string | null; decision: Record<string, unknown> | null;
  decision_ref: string | null; attempt_ref: string | null;
  dispatch_ref: string | null; clarification: Record<string, unknown> | null;
  delivery: Record<string, unknown> | null;
};
type OutcomeStatus = "success" | "partial" | "failure" | "aborted" | "unknown";
type OutcomeInput = {
  space_instance: string; attempt_ref: string;
  observer_configuration_digest: string; event_key: string;
  observed_at: string; metric: string; window: string;
  observation:
    | { kind: "measurement"; terminal: boolean; outcome_status: OutcomeStatus;
        magnitude?: number | null; payload: unknown }
    | { kind: "learning"; measurements: Record<string, unknown> };
  correction_of?: string | null; safety_signal?: string | null;
  utility?: { witness_ref?: string; witness?: ContributionWitness } | null;
};
type ObservationReceipt = {
  format: "anda-brain:observation-receipt-v1";
  receipt_id: string; scope: RuntimeScope; event_key: string;
  body_digest: string; observer: string; received_at_ms: number;
  observed_at: string; status: string;
  native_committed: boolean; learning_eligible: boolean;
  outcome_status: OutcomeStatus | null; outcome_ref: string | null;
  observation_ref: string | null; reason: string | null; safety_pending: boolean;
  safety_evaluation_ref?: string; // Native revocation that resolved/covered this signal.
};
type RuntimeStatus = {
  supported: boolean; configured: boolean; scope: RuntimeScope | null;
  attention_enabled: boolean; actions_enabled: boolean;
  observation_enabled: boolean; observer_authenticated: boolean;
  blocked_reasons: string[]; visible_items: number; inventory_complete: boolean;
  utility?: UtilityStatus;
  learning?: LearningRuntimeStatus;
  semantic_attention?: SemanticAttentionStatus;
  trust?: TrustRuntimeStatus;
};
```

### Observation and response semantics

Attention pages contain 1–50 items and at most 256 KiB of visible output. `complete`
means the end of this snapshot walk, not task completion or semantic completeness.
Each page rechecks current visibility, including native evidence behind gate packets.
MCP responses use `{id, response}` and listing uses `{cursor, limit}`. Neither reads
nor answers claim work. Clarifications require the committed ask, its recipient and
an unexpired deadline; a response enters a fresh gate without granting authority.
An `agent_statement` creates attributed Evidence and an Activity, never an independent
OutcomeRecord. For example:

```json
{"kind":"clarification","event_key":"answer-42","answer":"Tomorrow"}
```

A measurement body uses real retained identities and digests:

```json
{
  "space_instance":"sha256:INSTANCE_DIGEST",
  "attempt_ref":"X-42",
  "observer_configuration_digest":"sha256:REGISTERED_METHOD_DIGEST",
  "event_key":"instrument-event-42",
  "observed_at":"2026-09-17T10:00:00.000Z",
  "metric":"delivery",
  "window":"durable_inbox_v1",
  "observation":{
    "kind":"measurement",
    "terminal":true,
    "outcome_status":"success",
    "magnitude":null,
    "payload":{"delivery_digest":"sha256:ACTUAL_DELIVERY_REQUEST_DIGEST"}
  },
  "correction_of":null,
  "safety_signal":null
}
```

The observer contract pins principal, method/configuration digest, control domain,
task family, metric, window and allowed delay. Intake verifies the actual native
act/Attempt, transaction author, prior dispatch and observation time; the controller
cannot observe its own Attempt. Inbox success also requires the persisted delivery
and matching digest/time. It proves durable inbox delivery, not human reading or
business success. Other callbacks require their own independent measurement contract.

Accepted ordinary measurements atomically create Outcome Evidence and an
`outcome_observation` Activity. Raw material stays in Evidence payload, outside the
closed OutcomeRecord Facet. Nonterminal/unknown observations do not finish dispatch;
missing magnitude/cost remains missing. Terminal evidence reconciles the same
Attempt, including after cancellation. Native evidence and dispatch reconciliation
are separate recoverable steps; retry an unresolved write with the same event/body
and fresh authentication. Caller cancellation does not interrupt admitted writes.

Authenticated late, conflicting and nonterminal learning material remains auditable
without changing old trial eligibility. Corrections name an earlier event from the
same observer/Attempt and add new evidence; they never rewrite old Outcome/Evaluation
records. Safety signals remain discoverable when late/excluded. An explicitly enabled
learning consumer uses fresh observer authority, resolves native revocation, then
acknowledges `safety_pending` and records `safety_evaluation_ref`.

For registered learning Attempts, use `observation:{"kind":"learning", "measurements":{...}}`.
Membership comes from the retained ticket, including baseline Attempts with
`trial_ref:null`. Only `LearningRuntime::submit_outcome` writes the native result;
frozen cutoff, comparability and ACK recovery remain intact. An unavailable controller
causes refusal, never a generic fallback. `learning_eligible:true` means contract
acceptance, not a positive grade. Inputs are capped at 16 KiB and event keys at
256 bytes. See the [runtime guide](RUNTIME.md) for identity setup and storage bounds.

### Learning runtime status

The existing status route and MCP tool add `learning`; no cohort-enrollment or
observer-credential tool is exposed to a model. `capacity` and `last_pass` are null
unless the caller is an explicitly mapped auditor. Maintenance's optional
`skills.runtime` contains the separate scheduler status; its old Skill counters
do not count work from prior scheduler passes. See [the learning guide](LEARNING_RUNTIME.md).

```typescript
type LearningRuntimeStatus = {
  compiled: boolean; registered: boolean; registration_enabled?: boolean;
  bindings_ready: boolean; automatic_allowed: boolean; running?: boolean;
  automation?: { trials: boolean; reviews: boolean; archive: boolean; safety: boolean };
  blocked_reasons?: string[];
  capacity?: { hot_jobs: number; maximum_hot_jobs: number; archived_jobs: number;
    retained_identities: number; reserved_bytes: number;
    storage: { maximum_records: number; maximum_reserved_bytes: number } } | null;
  last_pass?: { started_at_ms: number; finished_at_ms: number; enrolled: number;
    driven: number; observed: number; settled: number; archived: number;
    reviews_checked: number; reviews_enrolled: number; safety_resolved: number;
    blocked: string[] } | null;
};
```

### Delivery and utility metadata

Structured Recall additionally returns optional `recall_receipt` transport metadata;
the answer/semantic packet remains unchanged. Plain/direct-agent calls retain a
receipt discoverable by their conversation ID through the trusted Rust API.
Outcome observers can provide exactly one inline witness or prior native witness
reference in `utility`; this never relaxes signed observer authentication. Runtime
status additionally reports `utility` switches, calibration state and recovery
reason. There is no model-facing setter or new MCP mutation tool. Detailed witness,
method and audit formats are in [UTILITY_RUNTIME.md](UTILITY_RUNTIME.md).

```typescript
type RecallReceiptRef = {
  id: string; digest: string; scope: RuntimeScope;
};
// RecallOutput.recall_receipt?: RecallReceiptRef
// OutcomeInput.utility?: { witness_ref?: string; witness?: ContributionWitness }
// RuntimeStatus.utility?: UtilityStatus
type UtilityStatus = {
  configured: boolean; automatic: boolean; apply: boolean;
  calibrated: boolean; ranking: boolean; running: boolean;
  reason: string | null;
};
```

### Semantic attention status

Runtime status adds optional `semantic_attention` with `configured`, `automatic`,
`running`, `pin`, `reason`, and nullable `last_pass`. The latter contains `scanned`,
`calls`, `input_tokens`, `advanced`, `fired`, `expired`, `deferred`, and `reason` for
one bounded pass, and is returned only to configured auditors. It is not complete
inventory or proof that all text Watches were evaluated. Existing JSON/CBOR/Markdown
and HTTP/MCP request shapes are unchanged. No model configuration or evaluation
write endpoint is added; operators install per-Space `semantic` startup bindings.
See [SEMANTIC_WATCH_RUNTIME.md](SEMANTIC_WATCH_RUNTIME.md) for budget, pin migration,
unknown/deferred semantics, native Artifact erasure and idempotent recovery.

```typescript
type SemanticAttentionStatus = {
  configured: boolean; automatic: boolean; running: boolean;
  pin: { id: string; digest: string } | null; reason: string | null;
  last_pass: {
    scanned: number; calls: number; input_tokens: number; advanced: number;
    fired: number; expired: number; deferred: number; reason: string | null;
  } | null;
};
```

### Contextual trust status and discovery hints

The existing HTTP status route and MCP status tool add optional `trust` metadata.
`governor_authorized` reflects the configured principal's current native authority;
it is not permission granted to the caller. No trust-management or independent-fact
writing tool is added to HTTP/MCP. Existing JSON/CBOR/Markdown requests remain valid.
A normal authenticated measurement payload can optionally include
`trust_verification_ref: "E-…"`; it announces an existing native factual verification
for bounded discovery, without converting action success/failure into a trust score.
Trusted Rust fact intake, review, application, version-conflict recovery and scoped
restoration are documented in [TRUST_RUNTIME.md](TRUST_RUNTIME.md).

```typescript
type TrustRuntimeStatus = {
  configured: boolean; automatic: boolean; apply: boolean; automatic_apply: boolean;
  calibrated: boolean; governor_authorized: boolean; running: boolean;
  reason: string | null;
};
```
