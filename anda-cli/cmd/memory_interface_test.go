package cmd

import (
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/ldclabs/anda-brain/anda-cli/api"
)

// A fake Brain that answers the Memory Interface: mutations are acknowledged
// with a receipt; a recall reports each after receipt available once
// "formation" has run, and echoes the attention cursor it was given.
func memoryServer(t *testing.T, requests *[]map[string]any) *httptest.Server {
	t.Helper()
	return httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		var request map[string]any
		_ = json.Unmarshal(body, &request)
		*requests = append(*requests, request)
		switch r.URL.Path {
		case "/v1/s/memory/sources":
			_, _ = w.Write([]byte(`{"result":{"source_ref":"src-1","source_digest":"sha256:x","captured_at":"2026-01-01T00:00:00.000Z"}}`))
		case "/v1/s/memory":
			operation, _ := request["operation"].(string)
			input, _ := request["input"].(map[string]any)
			if operation == "recall" {
				after := []map[string]any{}
				for _, receipt := range asStrings(input["after"]) {
					after = append(after, map[string]any{"receipt_ref": receipt, "phase": "available", "disposition": "formed", "resolved_seq": 3, "available_seq": 3})
				}
				briefing := map[string]any{"summary": "ok", "items": []any{}, "uncertainties": []any{}, "basis_ref": "basis-1",
					"coverage": map[string]any{"complete": true, "channels": map[string]string{}, "pending_receipts": []string{}, "action_eligible": true},
					"after":    after, "attention_cursor": "attention:9"}
				_ = json.NewEncoder(w).Encode(map[string]any{"kip_memory": "2.0", "operation": "recall", "status": "succeeded", "result": briefing, "warnings": []string{}})
				return
			}
			if operation == "forget" && input["mode"] == "semantic" {
				_, _ = w.Write([]byte(`{"kip_memory":"2.0","operation":"forget","status":"failed","error":{"code":"NotAuthorized","message":"owner only"},"warnings":[]}`))
				return
			}
			_ = json.NewEncoder(w).Encode(map[string]any{"kip_memory": "2.0", "operation": operation, "status": "pending",
				"receipt":  map[string]any{"receipt_ref": "rcpt-" + operation, "operation": operation, "space_id": "s", "accepted_seq": 1},
				"progress": map[string]any{"receipt_ref": "rcpt-" + operation, "phase": "recorded"},
				"result":   map[string]any{"summary": "Recorded", "memory_refs": []string{}}, "warnings": []string{}})
		default:
			http.NotFound(w, r)
		}
	}))
}

func asStrings(value any) []string {
	items, _ := value.([]any)
	out := []string{}
	for _, item := range items {
		if text, ok := item.(string); ok {
			out = append(out, text)
		}
	}
	return out
}

func TestMemoryInterfaceSessionKeepsReceiptsUntilARecallAccountsForThem(t *testing.T) {
	var requests []map[string]any
	server := memoryServer(t, &requests)
	defer server.Close()
	dir := t.TempDir()
	session := filepath.Join(dir, "session.json")
	env := map[string]string{}
	base := []string{"--base-url", server.URL, "--space-id", "s"}

	if _, stderr, err := runCLI(t, env, append(base, "memory", "sources", "stage", "--text", "I prefer dark mode", "--key", "k1")...); err != nil {
		t.Fatalf("stage: %v %s", err, stderr)
	}
	out, stderr, err := runCLI(t, env, append(base, "memory", "observe", "--source", "src-1", "--task", "t1", "--session", session)...)
	if err != nil {
		t.Fatalf("observe: %v %s", err, stderr)
	}
	if !strings.Contains(out, "rcpt-observe") {
		t.Fatalf("observe printed %s", out)
	}
	var saved memorySession
	data, _ := os.ReadFile(session)
	if err := json.Unmarshal(data, &saved); err != nil || len(saved.Outstanding) != 1 {
		t.Fatalf("session after observe: %s", data)
	}
	// The key is derived from the logical input, so a rerun replays.
	observe := requests[len(requests)-1]
	if key, _ := observe["idempotency_key"].(string); !strings.HasPrefix(key, "cli:observe:") {
		t.Fatalf("observe key %v", observe["idempotency_key"])
	}

	if _, stderr, err := runCLI(t, env, append(base, "memory", "recall", "What do I prefer?", "--mode", "attention", "--session", session)...); err != nil {
		t.Fatalf("recall: %v %s", err, stderr)
	}
	recall := requests[len(requests)-1]
	input, _ := recall["input"].(map[string]any)
	scope, _ := recall["scope"].(map[string]any)
	if scope["task_ref"] != "t1" {
		t.Fatalf("session lost its task scope: %v", recall["scope"])
	}
	if got := asStrings(input["after"]); len(got) != 1 || got[0] != "rcpt-observe" {
		t.Fatalf("recall after %v", input["after"])
	}
	data, _ = os.ReadFile(session)
	saved = memorySession{}
	_ = json.Unmarshal(data, &saved)
	if len(saved.Outstanding) != 0 || saved.AttentionCursor != "attention:9" {
		t.Fatalf("session after recall: %s", data)
	}

	// A failed intent is printed and fails the command.
	if _, _, err := runCLI(t, env, append(base, "memory", "forget", "A-1", "--mode", "semantic", "--session", session)...); err == nil {
		t.Fatal("a failed forget must exit non-zero")
	}
}

func TestMemoryInterfaceForgetRejectsDryRunBeforeSending(t *testing.T) {
	var requests []map[string]any
	server := memoryServer(t, &requests)
	defer server.Close()
	_, _, err := runCLI(t, map[string]string{}, "--base-url", server.URL, "--space-id", "s",
		"memory", "forget", "A-1", "--mode", "semantic", "--dry-run")
	if err == nil || len(requests) != 0 {
		t.Fatalf("dry-run submitted a mutation: err=%v requests=%v", err, requests)
	}
}

func TestMemoryKeyIsStableForTheSameLogicalInput(t *testing.T) {
	scope := &api.MemoryScope{TaskRef: "t"}
	first := memoryKey("observe", scope, map[string]any{"source_ref": "src-1"})
	if first != memoryKey("observe", scope, map[string]any{"source_ref": "src-1"}) {
		t.Fatal("same input must derive the same key")
	}
	if first == memoryKey("observe", nil, map[string]any{"source_ref": "src-1"}) {
		t.Fatal("a different scope must derive a different key")
	}
}

func TestMemorySessionRefusesOverflow(t *testing.T) {
	session := &memorySession{SpaceID: "s"}
	for i := 0; i < api.MaxAfter; i++ {
		if err := session.recordReceipt(&api.MemoryReceipt{ReceiptRef: "r" + string(rune('a'+i%26)) + string(rune('0'+i/26))}); err != nil {
			t.Fatal(err)
		}
	}
	if err := session.recordReceipt(&api.MemoryReceipt{ReceiptRef: "overflow"}); err == nil {
		t.Fatal("the 129th outstanding receipt must be refused, never dropped silently")
	}
}
