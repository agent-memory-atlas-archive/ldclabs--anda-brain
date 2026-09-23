package api

import (
	"context"
	"fmt"
	"net/http"
	"net/url"
	"strconv"
)

func callRPC[T any](ctx context.Context, c *Client, method, path string, input any) (*RpcResponse[T], error) {
	data, err := c.doJSON(ctx, method, path, input)
	if err != nil {
		return nil, err
	}
	var response RpcResponse[T]
	if err := DecodeJSON(data, &response); err != nil {
		return nil, fmt.Errorf("decode response: %w", err)
	}
	return &response, nil
}

func withQuery(path string, values url.Values) string {
	if len(values) == 0 {
		return path
	}
	return path + "?" + values.Encode()
}

func setLimit(values url.Values, limit int) {
	if limit > 0 {
		values.Set("limit", strconv.Itoa(limit))
	}
}

func (c *Client) RecallStructured(ctx context.Context, input *RecallInput) (*RpcResponse[RecallOutput], error) {
	return callRPC[RecallOutput](ctx, c, http.MethodPost, c.spacePath("/recall_structured"), input)
}

func (c *Client) Probe(ctx context.Context, input *ProbeInput) (*RpcResponse[ProbeOutput], error) {
	return callRPC[ProbeOutput](ctx, c, http.MethodPost, c.spacePath("/probe"), input)
}

func (c *Client) PinMemory(ctx context.Context, input *MemoryPinInput) (*RpcResponse[MemoryPinOutput], error) {
	return callRPC[MemoryPinOutput](ctx, c, http.MethodPost, c.spacePath("/memory/pin"), input)
}

func (c *Client) ForgetMemory(ctx context.Context, input *MemoryForgetInput) (*RpcResponse[MemoryForgetReport], error) {
	return callRPC[MemoryForgetReport](ctx, c, http.MethodPost, c.spacePath("/memory/forget"), input)
}

func (c *Client) GetMemoryStatus(ctx context.Context) (*RpcResponse[MemoryStatus], error) {
	return callRPC[MemoryStatus](ctx, c, http.MethodGet, c.spacePath("/memory_status"), nil)
}

func (c *Client) ShadowEval(ctx context.Context, input *ShadowEvalInput) (*RpcResponse[ShadowReport], error) {
	return callRPC[ShadowReport](ctx, c, http.MethodPost, c.spacePath("/management/shadow_eval"), input)
}

func (c *Client) WikiCommit(ctx context.Context, input *WikiCommitInput) (*RpcResponse[WikiCommitOutput], error) {
	return callRPC[WikiCommitOutput](ctx, c, http.MethodPost, c.spacePath("/wiki/docs"), input)
}

func (c *Client) WikiListDocs(ctx context.Context, query WikiListDocsQuery) (*RpcResponse[[]WikiDocInfo], error) {
	values := url.Values{}
	if query.Namespace != "" {
		values.Set("namespace", query.Namespace)
	}
	if query.Status != "" {
		values.Set("status", query.Status)
	}
	if query.Tag != "" {
		values.Set("tag", query.Tag)
	}
	if query.Cursor != "" {
		values.Set("cursor", query.Cursor)
	}
	setLimit(values, query.Limit)
	return callRPC[[]WikiDocInfo](ctx, c, http.MethodGet, withQuery(c.spacePath("/wiki/docs"), values), nil)
}

func (c *Client) WikiGetDoc(ctx context.Context, docID uint64) (*RpcResponse[WikiDocOutput], error) {
	return callRPC[WikiDocOutput](ctx, c, http.MethodGet, c.spacePath(fmt.Sprintf("/wiki/docs/%d", docID)), nil)
}

func (c *Client) WikiRead(ctx context.Context, docID uint64, query WikiReadQuery) (*RpcResponse[WikiReadOutput], error) {
	values := url.Values{}
	if query.Version != nil {
		values.Set("version", strconv.FormatUint(*query.Version, 10))
	}
	if query.Anchor != "" {
		values.Set("anchor", query.Anchor)
	}
	if query.Start != nil {
		values.Set("start", strconv.FormatUint(*query.Start, 10))
	}
	if query.End != nil {
		values.Set("end", strconv.FormatUint(*query.End, 10))
	}
	path := c.spacePath(fmt.Sprintf("/wiki/docs/%d/content", docID))
	return callRPC[WikiReadOutput](ctx, c, http.MethodGet, withQuery(path, values), nil)
}

func (c *Client) WikiVersions(ctx context.Context, docID uint64, cursor string, limit int) (*RpcResponse[[]WikiVersionInfo], error) {
	values := url.Values{}
	if cursor != "" {
		values.Set("cursor", cursor)
	}
	setLimit(values, limit)
	path := c.spacePath(fmt.Sprintf("/wiki/docs/%d/versions", docID))
	return callRPC[[]WikiVersionInfo](ctx, c, http.MethodGet, withQuery(path, values), nil)
}

func (c *Client) wikiSetArchived(ctx context.Context, docID uint64, archive bool) (*RpcResponse[WikiDocInfo], error) {
	action := "restore"
	if archive {
		action = "archive"
	}
	path := c.spacePath(fmt.Sprintf("/wiki/docs/%d/%s", docID, action))
	return callRPC[WikiDocInfo](ctx, c, http.MethodPost, path, nil)
}

func (c *Client) WikiArchive(ctx context.Context, docID uint64) (*RpcResponse[WikiDocInfo], error) {
	return c.wikiSetArchived(ctx, docID, true)
}

func (c *Client) WikiRestore(ctx context.Context, docID uint64) (*RpcResponse[WikiDocInfo], error) {
	return c.wikiSetArchived(ctx, docID, false)
}

func (c *Client) WikiSearch(ctx context.Context, input *WikiSearchInput) (*RpcResponse[WikiSearchOutput], error) {
	return callRPC[WikiSearchOutput](ctx, c, http.MethodPost, c.spacePath("/wiki/search"), input)
}

func (c *Client) WikiVerify(ctx context.Context, input *WikiVerifyInput) (*RpcResponse[WikiVerifyOutput], error) {
	return callRPC[WikiVerifyOutput](ctx, c, http.MethodPost, c.spacePath("/wiki/verify"), input)
}

func (c *Client) WikiEvents(ctx context.Context, query WikiEventsQuery) (*RpcResponse[[]WikiEventInfo], error) {
	values := url.Values{}
	if query.Kind != "" {
		values.Set("kind", query.Kind)
	}
	if query.DocID != nil {
		values.Set("doc_id", strconv.FormatUint(*query.DocID, 10))
	}
	if query.Cursor != "" {
		values.Set("cursor", query.Cursor)
	}
	setLimit(values, query.Limit)
	return callRPC[[]WikiEventInfo](ctx, c, http.MethodGet, withQuery(c.spacePath("/wiki/events"), values), nil)
}

func (c *Client) WikiImport(ctx context.Context, input *WikiImportInput) (*RpcResponse[WikiImportOutput], error) {
	return callRPC[WikiImportOutput](ctx, c, http.MethodPost, c.spacePath("/wiki/import"), input)
}

func (c *Client) WikiExport(ctx context.Context, namespace string) (*RpcResponse[WikiExportOutput], error) {
	values := url.Values{}
	if namespace != "" {
		values.Set("namespace", namespace)
	}
	return callRPC[WikiExportOutput](ctx, c, http.MethodGet, withQuery(c.spacePath("/wiki/export"), values), nil)
}

func (c *Client) WikiDigest(ctx context.Context) (*RpcResponse[WikiDigestReport], error) {
	return callRPC[WikiDigestReport](ctx, c, http.MethodPost, c.spacePath("/wiki/digest"), nil)
}
