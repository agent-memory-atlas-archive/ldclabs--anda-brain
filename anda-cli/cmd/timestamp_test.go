package cmd

import (
	"strings"
	"testing"
)

func TestSourceTimestampIsCanonicalMillisecondUTC(t *testing.T) {
	for input, want := range map[string]string{
		"":                            "",
		"2026-09-22T08:00:00+08:00":   "2026-09-22T00:00:00.000Z",
		" 2026-09-22T00:00:00.5Z ":    "2026-09-22T00:00:00.500Z",
		"2026-09-22T00:00:00.123000Z": "2026-09-22T00:00:00.123Z",
	} {
		got, err := sourceTimestamp(input)
		if err != nil || got != want {
			t.Fatalf("sourceTimestamp(%q) = %q, %v; want %q", input, got, err, want)
		}
	}
	for _, input := range []string{"yesterday", "2026-02-30T00:00:00Z", "2026-09-22T00:00:00.000123Z"} {
		if _, err := sourceTimestamp(input); err == nil {
			t.Fatalf("sourceTimestamp(%q) accepted a non-canonical instant", input)
		}
	}
}

func TestEachFileMessageKeepsItsOwnTime(t *testing.T) {
	messages, err := parseMessagesInput(`[
		{"role":"user","content":"We moved to Berlin.","timestamp":"2026-01-02T03:04:05+01:00"},
		{"role":"assistant","content":"Noted.","timestamp":1767319445500},
		{"role":"user","content":"No time here."}
	]`)
	if err != nil {
		t.Fatal(err)
	}
	if messages[0].Timestamp == nil || *messages[0].Timestamp != 1767319445000 {
		t.Fatalf("RFC 3339 message time = %v", messages[0].Timestamp)
	}
	if messages[1].Timestamp == nil || *messages[1].Timestamp != 1767319445500 {
		t.Fatalf("millisecond message time = %v", messages[1].Timestamp)
	}
	if messages[2].Timestamp != nil {
		t.Fatalf("a message without a time must not get one: %v", *messages[2].Timestamp)
	}
	single, err := parseMessagesInput(`{"role":"user","content":"One.","timestamp":"2026-01-02T02:04:05Z"}`)
	if err != nil || single[0].Timestamp == nil || *single[0].Timestamp != 1767319445000 {
		t.Fatalf("single message time = %v, %v", single, err)
	}
	for _, input := range []string{
		`[{"role":"user","content":"x","timestamp":"yesterday"}]`,
		`[{"role":"user","content":"x","timestamp":"2026-01-02T03:04:05.000123Z"}]`,
		`[{"role":"user","content":"x","timestamp":true}]`,
	} {
		if _, err := parseMessagesInput(input); err == nil || !strings.Contains(err.Error(), "timestamp") {
			t.Fatalf("parseMessagesInput(%s) = %v; want a timestamp error", input, err)
		}
	}
}
