package api

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strconv"
	"strings"
	"time"
)

const (
	DefaultBaseURL = "http://127.0.0.1:8042"
	DefaultTimeout = 120 * time.Second
)

// Client is the HTTP client for the Anda Brain API.
type Client struct {
	BaseURL string
	SpaceID string
	Token   string
	// Shard is sent as the "Shard-Id" header when > 0, for sharded deployments.
	Shard      int
	HTTPClient *http.Client
}

// NewClient creates a new API client.
func NewClient(baseURL, spaceID, token string) *Client {
	baseURL = strings.TrimRight(baseURL, "/")
	return &Client{
		BaseURL: baseURL,
		SpaceID: spaceID,
		Token:   token,
		HTTPClient: &http.Client{
			Timeout: DefaultTimeout,
		},
	}
}

func (c *Client) spacePath(path string) string {
	return fmt.Sprintf("/v1/%s%s", url.PathEscape(c.SpaceID), path)
}

func (c *Client) doJSON(ctx context.Context, method, path string, body any) ([]byte, error) {
	var reqBody io.Reader
	if body != nil {
		data, err := json.Marshal(body)
		if err != nil {
			return nil, fmt.Errorf("marshal request: %w", err)
		}
		reqBody = bytes.NewReader(data)
	}

	reqURL := c.BaseURL + path
	req, err := http.NewRequestWithContext(ctx, method, reqURL, reqBody)
	if err != nil {
		return nil, fmt.Errorf("create request: %w", err)
	}

	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	req.Header.Set("Accept", "application/json")
	if c.Token != "" {
		req.Header.Set("Authorization", "Bearer "+c.Token)
	}
	if c.Shard > 0 {
		req.Header.Set("Shard-Id", strconv.Itoa(c.Shard))
	}

	resp, err := c.HTTPClient.Do(req)
	if err != nil {
		return nil, fmt.Errorf("http request: %w", err)
	}
	defer resp.Body.Close()

	respBody, err := io.ReadAll(resp.Body)
	if err != nil {
		return nil, fmt.Errorf("read response: %w", err)
	}

	if resp.StatusCode >= 400 {
		var rpcErr RpcError
		if DecodeJSON(respBody, &rpcErr) == nil && rpcErr.Message != "" {
			return nil, &HTTPError{StatusCode: resp.StatusCode, RPC: &rpcErr}
		}
		return nil, &HTTPError{StatusCode: resp.StatusCode, Body: string(respBody)}
	}

	return respBody, nil
}

// GetInfo returns service information.
func (c *Client) GetInfo(ctx context.Context) (*ServiceInfo, error) {
	data, err := c.doJSON(ctx, http.MethodGet, "/info", nil)
	if err != nil {
		return nil, err
	}
	var info ServiceInfo
	if err := DecodeJSON(data, &info); err != nil {
		return nil, fmt.Errorf("decode response: %w", err)
	}
	return &info, nil
}

// Formation submits a memory formation task.
func (c *Client) Formation(ctx context.Context, input *FormationInput) (*RpcResponse[AgentOutput], error) {
	return callRPC[AgentOutput](ctx, c, http.MethodPost, c.spacePath("/formation"), input)
}

// Recall queries memory with natural language.
func (c *Client) Recall(ctx context.Context, input *RecallInput) (*RpcResponse[AgentOutput], error) {
	return callRPC[AgentOutput](ctx, c, http.MethodPost, c.spacePath("/recall"), input)
}

// Maintenance triggers maintenance task.
func (c *Client) Maintenance(ctx context.Context, input *MaintenanceInput) (*RpcResponse[AgentOutput], error) {
	return callRPC[AgentOutput](ctx, c, http.MethodPost, c.spacePath("/maintenance"), input)
}

// ExecuteKIPReadonly executes a KIP request in read-only mode.
func (c *Client) ExecuteKIPReadonly(ctx context.Context, input *KipRequest) (*KipResponse[any], error) {
	data, err := c.doJSON(ctx, http.MethodPost, c.spacePath("/execute_kip_readonly"), input)
	if err != nil {
		return nil, err
	}
	var resp KipResponse[any]
	if err := DecodeJSON(data, &resp); err != nil {
		return nil, fmt.Errorf("decode response: %w", err)
	}
	return &resp, nil
}

// GetOrInitUser gets or initializes a caller concept.
func (c *Client) GetOrInitUser(ctx context.Context, input *GetOrInitUserInput) (*RpcResponse[Concept], error) {
	response, err := callRPC[Concept](ctx, c, http.MethodPost, c.spacePath("/get_or_init_user"), input)
	if err != nil {
		return nil, err
	}
	// Preserve this method's existing Go error contract for library callers.
	if response.Error != nil {
		return nil, fmt.Errorf("RPC error: %w", response.Error)
	}
	return response, nil
}

// GetSpaceInfo returns space information.
func (c *Client) GetSpaceInfo(ctx context.Context) (*RpcResponse[SpaceInfo], error) {
	return callRPC[SpaceInfo](ctx, c, http.MethodGet, c.spacePath("/info"), nil)
}

func (c *Client) GetFormationStatus(ctx context.Context) (*RpcResponse[FormationStatus], error) {
	return callRPC[FormationStatus](ctx, c, http.MethodGet, c.spacePath("/formation_status"), nil)
}

// GetConversation returns a single conversation.
func (c *Client) GetConversation(ctx context.Context, conversationID uint64, collection string) (*RpcResponse[Conversation], error) {
	path := fmt.Sprintf("%s/conversations/%d", c.spacePath(""), conversationID)
	params := url.Values{}
	if collection != "" {
		params.Set("collection", collection)
	}
	if len(params) > 0 {
		path += "?" + params.Encode()
	}
	return callRPC[Conversation](ctx, c, http.MethodGet, path, nil)
}

// GetConversationDelta returns incremental conversation updates since the given offsets.
func (c *Client) GetConversationDelta(ctx context.Context, conversationID uint64, messagesOffset, artifactsOffset int, collection string) (*RpcResponse[ConversationDelta], error) {
	path := fmt.Sprintf("%s/conversations/%d/delta", c.spacePath(""), conversationID)
	params := url.Values{}
	if messagesOffset > 0 {
		params.Set("messages_offset", fmt.Sprintf("%d", messagesOffset))
	}
	if artifactsOffset > 0 {
		params.Set("artifacts_offset", fmt.Sprintf("%d", artifactsOffset))
	}
	if collection != "" {
		params.Set("collection", collection)
	}
	if len(params) > 0 {
		path += "?" + params.Encode()
	}

	return callRPC[ConversationDelta](ctx, c, http.MethodGet, path, nil)
}

// ListConversations lists conversations with pagination.
func (c *Client) ListConversations(ctx context.Context, cursor string, limit int, collection string) (*RpcResponse[[]Conversation], error) {
	path := c.spacePath("/conversations")
	params := url.Values{}
	if cursor != "" {
		params.Set("cursor", cursor)
	}
	if limit > 0 {
		params.Set("limit", fmt.Sprintf("%d", limit))
	}
	if collection != "" {
		params.Set("collection", collection)
	}
	if len(params) > 0 {
		path += "?" + params.Encode()
	}

	return callRPC[[]Conversation](ctx, c, http.MethodGet, path, nil)
}

// ListSpaceTokens lists space tokens (management).
func (c *Client) ListSpaceTokens(ctx context.Context) (*RpcResponse[[]SpaceToken], error) {
	return callRPC[[]SpaceToken](ctx, c, http.MethodGet, c.spacePath("/management/space_tokens"), nil)
}

// AddSpaceToken adds a space token (management).
func (c *Client) AddSpaceToken(ctx context.Context, input *AddSpaceTokenInput) (*RpcResponse[SpaceToken], error) {
	return callRPC[SpaceToken](ctx, c, http.MethodPost, c.spacePath("/management/add_space_token"), input)
}

// RevokeSpaceToken revokes a space token by its full value (management).
func (c *Client) RevokeSpaceToken(ctx context.Context, token string) (*RpcResponse[bool], error) {
	return c.revokeSpaceToken(ctx, RevokeSpaceTokenInput{Token: token})
}

// RevokeSpaceTokenByName revokes a space token by its unique name (management).
// This is the recovery path when the full token value was not saved at mint
// time: ListSpaceTokens only echoes a display prefix.
func (c *Client) RevokeSpaceTokenByName(ctx context.Context, name string) (*RpcResponse[bool], error) {
	return c.revokeSpaceToken(ctx, RevokeSpaceTokenInput{Name: name})
}

func (c *Client) revokeSpaceToken(ctx context.Context, input RevokeSpaceTokenInput) (*RpcResponse[bool], error) {
	return callRPC[bool](ctx, c, http.MethodPost, c.spacePath("/management/revoke_space_token"), input)
}

// UpdateSpace updates space information (management).
func (c *Client) UpdateSpace(ctx context.Context, input *UpdateSpaceInput) (*RpcResponse[bool], error) {
	return callRPC[bool](ctx, c, http.MethodPatch, c.spacePath("/management/update_space"), input)
}

func (c *Client) RestartFormation(ctx context.Context, input *RestartFormationInput) (*RpcResponse[bool], error) {
	return callRPC[bool](ctx, c, http.MethodPatch, c.spacePath("/management/restart_formation"), input)
}

func (c *Client) GetBYOK(ctx context.Context) (*RpcResponse[ModelConfig], error) {
	return callRPC[ModelConfig](ctx, c, http.MethodGet, c.spacePath("/management/space_byok"), nil)
}

func (c *Client) UpdateBYOK(ctx context.Context, input *ModelConfig) (*RpcResponse[bool], error) {
	return callRPC[bool](ctx, c, http.MethodPatch, c.spacePath("/management/space_byok"), input)
}

// CreateSpace creates a space (admin).
func (c *Client) CreateSpace(ctx context.Context, input *CreateOrUpdateSpaceInput) (*RpcResponse[SpaceInfo], error) {
	return callRPC[SpaceInfo](ctx, c, http.MethodPost, "/admin/create_space", input)
}

// UpdateSpaceTier updates space tier (admin).
func (c *Client) UpdateSpaceTier(ctx context.Context, spaceID string, input *CreateOrUpdateSpaceInput) (*RpcResponse[SpaceTier], error) {
	path := fmt.Sprintf("/admin/%s/update_space_tier", url.PathEscape(spaceID))
	return callRPC[SpaceTier](ctx, c, http.MethodPost, path, input)
}
