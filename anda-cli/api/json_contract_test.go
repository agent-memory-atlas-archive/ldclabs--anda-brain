package api

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

func TestContentPartPreservesErrorAndNumbers(t *testing.T) {
	for _, flag := range []string{`,"isError":true`, `,"isError":false`, ""} {
		raw := `[{"type":"ToolOutput","name":"lookup","output":{"id":9007199254740993}` + flag + `}]`
		var content MessageContent
		if err := json.Unmarshal([]byte(raw), &content); err != nil {
			t.Fatal(err)
		}
		data, err := json.Marshal(content)
		if err != nil {
			t.Fatal(err)
		}
		if string(data) != raw {
			t.Fatalf("payload changed: %s -> %s", raw, data)
		}
	}
}

func TestNestedDynamicNumbersSurviveCustomCodecs(t *testing.T) {
	for _, raw := range []string{
		`[{"type":"ToolCall","name":"lookup","args":{"id":9007199254740993}}]`,
		`[{"type":"Action","name":"lookup","payload":{"id":9007199254740993}}]`,
	} {
		var content MessageContent
		if err := json.Unmarshal([]byte(raw), &content); err != nil {
			t.Fatal(err)
		}
		data, err := json.Marshal(content)
		if err != nil || string(data) != raw {
			t.Fatalf("payload changed: %s (%v)", data, err)
		}
	}
	var concept Concept
	if err := json.Unmarshal([]byte(`{"id":"C-7","attributes":{"id":9007199254740993},"governance":{"version":9007199254740993}}`), &concept); err != nil {
		t.Fatal(err)
	}
	data, err := json.Marshal(concept)
	if err != nil || strings.Count(string(data), "9007199254740993") != 2 {
		t.Fatalf("concept numbers lost: %s (%v)", data, err)
	}
}

func TestCurrentResponseFieldsRoundTrip(t *testing.T) {
	// These are the public fields serialized by the current Rust host and
	// anda_core/anda_engine, including values that the older CLI discarded.
	cases := []struct {
		raw    string
		value  any
		fields []string
	}{
		{`{"answer":"ok","found":true,"usage":{},"recall_receipt":{"id":"receipt-1","digest":"sha256:abc","scope":{"space_id":"s1","space_instance":"instance-1"}}}`, &RecallOutput{}, []string{`"recall_receipt"`, `"space_instance":"instance-1"`}},
		{`{"digested":1,"failed":2,"usage":{}}`, &WikiDigestReport{}, []string{`"failed":2`}},
		{`{"_id":7,"child":8,"extra":{"id":9007199254740993}}`, &Conversation{}, []string{`"child":8`, `"extra":{"id":9007199254740993}`}},
		{`{"content":"ok","thoughts":"reasoning","tools_usage":{"search":{"requests":1}},"tool_calls":[{"name":"search","args":{"id":9007199254740993}}],"chat_history":[{"role":"tool","content":[{"type":"ToolOutput","name":"search","output":{},"isError":true}]}],"artifacts":[{"id":9007199254740993}],"session":"s1"}`, &AgentOutput{}, []string{`"thoughts"`, `"tools_usage"`, `"tool_calls"`, `"chat_history"`, `"isError":true`, `"artifacts"`, `"session":"s1"`}},
	}
	for _, tc := range cases {
		if err := DecodeJSON([]byte(tc.raw), tc.value); err != nil {
			t.Fatal(err)
		}
		data, err := json.Marshal(tc.value)
		if err != nil {
			t.Fatal(err)
		}
		for _, field := range tc.fields {
			if !strings.Contains(string(data), field) {
				t.Fatalf("missing %s in %s", field, data)
			}
		}
	}
}

func TestClientPreservesKIPNumbers(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = w.Write([]byte(`{"kip":"2.0","status":"succeeded","results":[{"status":"succeeded","result":{"id":9007199254740993}}]}`))
	}))
	defer server.Close()
	result, err := NewClient(server.URL, "s1", "").ExecuteKIPReadonly(context.Background(), &KipRequest{Command: "DESCRIBE PRIMER"})
	if err != nil {
		t.Fatal(err)
	}
	data, err := json.Marshal(result)
	if err != nil || !strings.Contains(string(data), "9007199254740993") {
		t.Fatalf("result changed: %s (%v)", data, err)
	}
}
