package api

import "fmt"

// Failure reports execution failure without discarding the output or usage.
func (r AgentOutput) Failure() error {
	if r.FailedReason != "" {
		return fmt.Errorf("agent failed: %s", r.FailedReason)
	}
	return nil
}

func (r RecallOutput) Failure() error {
	if r.FailedReason != "" {
		return fmt.Errorf("recall failed: %s", r.FailedReason)
	}
	return nil
}

func (r MemoryForgetReport) Failure() error {
	failed := 0
	for _, entity := range r.Entities {
		if entity.Error != "" {
			failed++
		}
	}
	if failed > 0 {
		return fmt.Errorf("memory forget failed for %d entities; see the JSON report", failed)
	}
	return nil
}

func (r WikiDigestReport) Failure() error {
	if r.Failed > 0 {
		return fmt.Errorf("wiki digest failed for %d documents; they remain queued", r.Failed)
	}
	return nil
}
