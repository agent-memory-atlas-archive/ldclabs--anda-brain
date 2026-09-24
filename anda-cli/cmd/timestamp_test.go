package cmd

import "testing"

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
