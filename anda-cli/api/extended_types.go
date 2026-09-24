package api

import "encoding/json"

// RecallBudget opts into a bounded memory packet. Omitted members use server defaults.
type RecallBudget struct {
	Tokenizer     string  `json:"tokenizer,omitempty"`
	MaxTokens     *uint32 `json:"max_tokens,omitempty"`
	ContextTokens *uint32 `json:"context_tokens,omitempty"`
}

// MemoryPolicy is replaced as a whole by update_space. Pointer fields let callers
// omit members so the server can apply its compiled defaults.
type MemoryPolicy struct {
	Version                   *uint32       `json:"version,omitempty"`
	MemoryStrengthDecayFactor *float64      `json:"memory_strength_decay_factor,omitempty"` // Deprecated: nothing reads it.
	RecallReinforcement       *float64      `json:"recall_reinforcement,omitempty"`
	CorrectionPenalty         *float64      `json:"correction_penalty,omitempty"`
	DecayFloor                *float64      `json:"decay_floor,omitempty"` // Deprecated: nothing reads it.
	StaleEventThresholdDays   *uint32       `json:"stale_event_threshold_days,omitempty"`
	UnconsolidatedMaxBacklog  *uint32       `json:"unconsolidated_max_backlog,omitempty"`
	OrphanMaxCount            *uint32       `json:"orphan_max_count,omitempty"`
	SelfTestQueriesPerCycle   *uint32       `json:"self_test_queries_per_cycle,omitempty"`
	SelfTestTokenBudget       *uint64       `json:"self_test_token_budget,omitempty"`
	RecallSearchThreshold     *float64      `json:"recall_search_threshold,omitempty"`
	RecallMaxRounds           *uint32       `json:"recall_max_rounds,omitempty"`
	RecallBudget              *RecallBudget `json:"recall_budget,omitempty"`
	ShadowReplaySample        *uint32       `json:"shadow_replay_sample,omitempty"`
}

type MemoryCitation struct {
	Entity     string   `json:"entity"`
	Type       string   `json:"type,omitempty"`
	Name       string   `json:"name,omitempty"`
	Confidence *float64 `json:"confidence,omitempty"`
	Source     string   `json:"source,omitempty"`
	CreatedAt  string   `json:"created_at,omitempty"`
}

type RecallReceiptRef struct {
	ID     string             `json:"id"`
	Digest string             `json:"digest"`
	Scope  RecallReceiptScope `json:"scope"`
}

type RecallReceiptScope struct {
	SpaceID       string `json:"space_id"`
	SpaceInstance string `json:"space_instance"`
}

type RecallOutput struct {
	RecallReceipt *RecallReceiptRef    `json:"recall_receipt,omitempty"`
	Answer        string               `json:"answer"`
	Found         bool                 `json:"found"`
	Uncertainty   *float64             `json:"uncertainty,omitempty"`
	Memories      []MemoryCitation     `json:"memories,omitempty"`
	Conversation  *uint64              `json:"conversation,omitempty"`
	Usage         Usage                `json:"usage"`
	FailedReason  string               `json:"failed_reason,omitempty"`
	MemoryBudget  *RecallBudgetReceipt `json:"memory_budget,omitempty"`
}

type RecallBudgetReceipt struct {
	Tokenizer         string `json:"tokenizer"`
	TokenLimit        uint32 `json:"token_limit"`
	Tokens            uint64 `json:"tokens"`
	ContextTokenLimit uint32 `json:"context_token_limit"`
}

type ProbeInput struct {
	Query string `json:"query"`
	Limit *int   `json:"limit,omitempty"`
}

type ProbeOutput struct {
	Found            bool             `json:"found"`
	NegativeCached   bool             `json:"negative_cached"`
	SearchExhaustive *bool            `json:"search_exhaustive,omitempty"`
	Hits             []MemoryCitation `json:"hits,omitempty"`
}

type MemoryPinInput struct {
	Entity string `json:"entity"`
	Pinned *bool  `json:"pinned,omitempty"` // omitted means true
}

type MemoryPinOutput struct {
	Entity  string `json:"entity"`
	Pinned  bool   `json:"pinned"`
	Updated uint64 `json:"updated"`
}

type MemoryForgetInput struct {
	Entities []string `json:"entities"`
	DryRun   bool     `json:"dry_run"`
}

type MemoryForgetEntity struct {
	Entity  string `json:"entity"`
	Existed bool   `json:"existed"`
	Error   string `json:"error,omitempty"`
}

type MemoryForgetReport struct {
	DryRun              bool                 `json:"dry_run"`
	DeletedConcepts     uint64               `json:"deleted_concepts"`
	DeletedPropositions uint64               `json:"deleted_propositions"`
	DeletedAssertions   uint64               `json:"deleted_assertions"`
	DeletedEvidence     uint64               `json:"deleted_evidence"`
	DeletedActivities   uint64               `json:"deleted_activities"`
	Entities            []MemoryForgetEntity `json:"entities,omitempty"`
}

type MemoryMetrics struct {
	RecallsCompleted   uint64  `json:"recalls_completed"`
	EntitiesRecalled   uint64  `json:"entities_recalled"`
	ProbeHits          uint64  `json:"probe_hits"`
	ProbeMisses        uint64  `json:"probe_misses"`
	NegativeCacheHits  uint64  `json:"negative_cache_hits"`
	SelfTestTested     uint64  `json:"self_test_tested"`
	SelfTestGrounded   uint64  `json:"self_test_grounded"`
	ReencodeTasks      uint64  `json:"reencode_tasks"`
	Corrections        uint64  `json:"corrections"`
	UncertaintyReports uint64  `json:"uncertainty_reports"`
	UncertaintySum     float64 `json:"uncertainty_sum"`
	ForgottenEntities  uint64  `json:"forgotten_entities"`
	UpdatedAt          uint64  `json:"updated_at"`
}

type MemoryGraphCounters struct {
	Concepts       uint64  `json:"concepts"`
	Propositions   uint64  `json:"propositions"`
	Unconsolidated *uint64 `json:"unconsolidated,omitempty"`
	Orphans        *uint64 `json:"orphans,omitempty"`
	PredicateTypes *uint64 `json:"predicate_types,omitempty"`
	AsOf           *uint64 `json:"as_of,omitempty"`
}

type MemoryStatus struct {
	Metrics                    MemoryMetrics       `json:"metrics"`
	Groundability              *float64            `json:"groundability,omitempty"`
	ProbeHitRate               *float64            `json:"probe_hit_rate,omitempty"`
	CorrectionRate             *float64            `json:"correction_rate,omitempty"`
	AvgUncertainty             *float64            `json:"avg_uncertainty,omitempty"`
	MaintenanceTokensPerRecall *float64            `json:"maintenance_tokens_per_recall,omitempty"`
	Graph                      MemoryGraphCounters `json:"graph"`
	LastSettlement             json.RawMessage     `json:"last_settlement,omitempty"`
	LastSelfTest               json.RawMessage     `json:"last_self_test,omitempty"`
	LastShadow                 *ShadowReport       `json:"last_shadow,omitempty"`
	LastSchemaAudit            json.RawMessage     `json:"last_schema_audit,omitempty"`
}

type ShadowEvalInput struct {
	Policy       MemoryPolicy `json:"policy"`
	ReplaySample *int         `json:"replay_sample,omitempty"`
}

type ShadowSample struct {
	Query  string `json:"query"`
	Winner string `json:"winner"`
	Reason string `json:"reason,omitempty"`
}

type ShadowReport struct {
	ComparedAt      uint64         `json:"compared_at"`
	Replayed        uint64         `json:"replayed"`
	BaselineWins    uint64         `json:"baseline_wins"`
	CandidateWins   uint64         `json:"candidate_wins"`
	Ties            uint64         `json:"ties"`
	JudgeErrors     uint64         `json:"judge_errors"`
	CandidatePolicy MemoryPolicy   `json:"candidate_policy"`
	Usage           Usage          `json:"usage"`
	Samples         []ShadowSample `json:"samples,omitempty"`
}

type WikiCommitInput struct {
	DocID         *uint64         `json:"doc_id,omitempty"`
	ParentVersion *uint64         `json:"parent_version,omitempty"`
	Namespace     string          `json:"namespace,omitempty"`
	Slug          string          `json:"slug,omitempty"`
	Title         string          `json:"title"`
	Content       string          `json:"content"`
	Tags          *[]string       `json:"tags,omitempty"`
	ACLLabel      *string         `json:"acl_label,omitempty"`
	SourceURI     *string         `json:"source_uri,omitempty"`
	Message       *string         `json:"message,omitempty"`
	Metadata      *map[string]any `json:"metadata,omitempty"`
}

type WikiDocInfo struct {
	ID              uint64         `json:"id"`
	Namespace       string         `json:"namespace"`
	Slug            string         `json:"slug"`
	Title           string         `json:"title"`
	Status          string         `json:"status"`
	CurrentVersion  uint64         `json:"current_version"`
	CurrentChecksum string         `json:"current_checksum"`
	Tags            []string       `json:"tags"`
	ACLLabel        string         `json:"acl_label,omitempty"`
	SourceURI       string         `json:"source_uri,omitempty"`
	Metadata        map[string]any `json:"metadata,omitempty"`
	CreatedBy       string         `json:"created_by"`
	UpdatedBy       string         `json:"updated_by"`
	CreatedAt       uint64         `json:"created_at"`
	UpdatedAt       uint64         `json:"updated_at"`
}

type WikiVersionInfo struct {
	ID            uint64  `json:"id"`
	DocID         uint64  `json:"doc_id"`
	ParentVersion *uint64 `json:"parent_version,omitempty"`
	Checksum      string  `json:"checksum"`
	Size          uint64  `json:"size"`
	Author        string  `json:"author"`
	Message       string  `json:"message,omitempty"`
	CreatedAt     uint64  `json:"created_at"`
}

type WikiCommitOutput struct {
	Doc        WikiDocInfo     `json:"doc"`
	Version    WikiVersionInfo `json:"version"`
	Chunks     int             `json:"chunks"`
	Created    bool            `json:"created"`
	Idempotent bool            `json:"idempotent"`
}

type WikiListDocsQuery struct {
	Namespace string
	Status    string
	Tag       string
	Cursor    string
	Limit     int
}

type WikiSearchInput struct {
	Query      string   `json:"query"`
	Namespaces []string `json:"namespaces,omitempty"`
	DocIDs     []uint64 `json:"doc_ids,omitempty"`
	Tags       []string `json:"tags,omitempty"`
	TopK       *int     `json:"top_k,omitempty"`
	Mode       string   `json:"mode,omitempty"`
	Expand     *uint8   `json:"expand,omitempty"`
}

type WikiCitation struct {
	URI         string    `json:"uri"`
	DocID       uint64    `json:"doc_id"`
	VersionID   uint64    `json:"version_id"`
	ChunkID     uint64    `json:"chunk_id"`
	HeadingPath []string  `json:"heading_path"`
	Anchor      string    `json:"anchor"`
	ByteRange   [2]uint64 `json:"byte_range"`
	Checksum    string    `json:"checksum"`
	Quote       string    `json:"quote"`
}

type WikiHit struct {
	Text        string       `json:"text"`
	DocTitle    string       `json:"doc_title"`
	HeadingPath []string     `json:"heading_path"`
	Citation    WikiCitation `json:"citation"`
}

type WikiSearchOutput struct {
	Hits             []WikiHit `json:"hits"`
	TotalDocsMatched int       `json:"total_docs_matched"`
}

type WikiTocEntry struct {
	Anchor      string   `json:"anchor"`
	HeadingPath []string `json:"heading_path"`
	ByteStart   uint64   `json:"byte_start"`
	ByteEnd     uint64   `json:"byte_end"`
}

type WikiDocOutput struct {
	Doc WikiDocInfo    `json:"doc"`
	Toc []WikiTocEntry `json:"toc"`
}

type WikiReadQuery struct {
	Version *uint64
	Anchor  string
	Start   *uint64
	End     *uint64
}

type WikiReadOutput struct {
	DocID     uint64          `json:"doc_id"`
	VersionID uint64          `json:"version_id"`
	IsCurrent bool            `json:"is_current"`
	Title     string          `json:"title"`
	Status    string          `json:"status"`
	Checksum  string          `json:"checksum"`
	Size      uint64          `json:"size"`
	Toc       *[]WikiTocEntry `json:"toc,omitempty"`
	Content   *string         `json:"content,omitempty"`
	ByteRange *[2]uint64      `json:"byte_range,omitempty"`
	Truncated bool            `json:"truncated"`
}

type WikiVerifyInput struct {
	URI       string     `json:"uri,omitempty"`
	DocID     *uint64    `json:"doc_id,omitempty"`
	VersionID *uint64    `json:"version_id,omitempty"`
	ByteRange *[2]uint64 `json:"byte_range,omitempty"`
	Checksum  string     `json:"checksum,omitempty"`
}

type WikiVerifyOutput struct {
	Status         string  `json:"status"`
	CurrentVersion *uint64 `json:"current_version,omitempty"`
	Checksum       string  `json:"checksum,omitempty"`
	Quote          string  `json:"quote,omitempty"`
}

type WikiEventsQuery struct {
	Kind   string
	DocID  *uint64
	Cursor string
	Limit  int
}

type WikiEventInfo struct {
	ID        uint64         `json:"id"`
	Kind      string         `json:"kind"`
	DocID     *uint64        `json:"doc_id,omitempty"`
	VersionID *uint64        `json:"version_id,omitempty"`
	Actor     string         `json:"actor"`
	Detail    map[string]any `json:"detail,omitempty"`
	CreatedAt uint64         `json:"created_at"`
}

type WikiBundleEntry struct {
	Path    string `json:"path"`
	Content string `json:"content"`
}

type WikiImportInput struct {
	Entries   []WikiBundleEntry `json:"entries"`
	Namespace string            `json:"namespace,omitempty"`
}

type WikiImportedDoc struct {
	Path      string `json:"path"`
	DocID     uint64 `json:"doc_id"`
	VersionID uint64 `json:"version_id"`
	Status    string `json:"status"`
}

type WikiImportSkip struct {
	Path   string `json:"path"`
	Reason string `json:"reason"`
}

type WikiImportOutput struct {
	Created   int               `json:"created"`
	Updated   int               `json:"updated"`
	Unchanged int               `json:"unchanged"`
	Docs      []WikiImportedDoc `json:"docs"`
	Skipped   []WikiImportSkip  `json:"skipped,omitempty"`
}

type WikiExportOutput struct {
	Namespace string            `json:"namespace"`
	Entries   []WikiBundleEntry `json:"entries"`
	Docs      int               `json:"docs"`
}

type WikiDigestReport struct {
	Failed           int   `json:"failed"`
	Digested         int   `json:"digested"`
	Facts            int   `json:"facts"`
	Superseded       int   `json:"superseded"`
	Skipped          int   `json:"skipped"`
	CitationsChecked int   `json:"citations_checked"`
	CitationsInvalid int   `json:"citations_invalid"`
	Usage            Usage `json:"usage"`
}
