package cmd

import (
	"strings"
	"testing"

	"github.com/ldclabs/anda-brain/anda-cli/api"
)

func TestCurrentAPICommandsAreRegistered(t *testing.T) {
	for _, path := range []string{
		"recall", "probe", "memory-status", "memory pin", "memory forget",
		"management shadow-eval", "wiki commit", "wiki list", "wiki get",
		"wiki read", "wiki versions", "wiki archive", "wiki restore",
		"wiki search", "wiki verify", "wiki events", "wiki import",
		"wiki export", "wiki digest", "schema drafts", "schema promote",
	} {
		parts := strings.Fields(path)
		command, _, err := rootCmd.Find(parts)
		if err != nil || command == nil || command.Name() != parts[len(parts)-1] {
			t.Fatalf("missing command %q: command=%v err=%v", path, command, err)
		}
	}
}

func TestPolicyJSONInputPreservesOptionalMembers(t *testing.T) {
	policy, err := readJSONObject[api.MemoryPolicy](`{"recall_max_rounds":8}`)
	if err != nil || policy.RecallMaxRounds == nil || *policy.RecallMaxRounds != 8 {
		t.Fatalf("policy parse failed: %+v, %v", policy, err)
	}
	if policy.MemoryStrengthDecayFactor != nil {
		t.Fatal("omitted policy member became an explicit value")
	}
	if _, err := readJSONObject[api.MemoryPolicy](`{"unknown_knob":1}`); err == nil {
		t.Fatal("unknown policy field silently dropped")
	}
}
