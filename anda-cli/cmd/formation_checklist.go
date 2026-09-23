package cmd

import (
	"bufio"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/ldclabs/anda-brain/anda-cli/api"
)

const defaultBatchReportFileName = ".formation-batch-checklist.json"
const (
	batchStatusPending   = "pending"
	batchStatusWorking   = "working"
	batchStatusSubmitted = "submitted"
	batchStatusSucceeded = "succeeded" // Old checklist submission status.
	batchStatusFailed    = "failed"
)

type formationTarget struct {
	BaseURL string `json:"base_url"`
	SpaceID string `json:"space_id"`
	Shard   int    `json:"shard"`
}

type fileFormationChecklist struct {
	RootDir   string                                  `json:"root_dir"`
	Selector  string                                  `json:"selector"`
	Target    *formationTarget                        `json:"target,omitempty"`
	UpdatedAt string                                  `json:"updated_at"`
	Entries   map[string]*fileFormationChecklistEntry `json:"entries"`
}

type fileFormationChecklistEntry struct {
	Path         string  `json:"path"`
	Status       string  `json:"status"`
	Digest       string  `json:"digest,omitempty"`
	Attempts     int     `json:"attempts"`
	LastError    string  `json:"last_error,omitempty"`
	UpdatedAt    string  `json:"updated_at"`
	Conversation *uint64 `json:"conversation,omitempty"`
}

func (c *fileFormationChecklist) bindTarget(client *api.Client) error {
	endpoint, err := url.Parse(client.BaseURL)
	if err != nil || endpoint.Host == "" || (endpoint.Scheme != "http" && endpoint.Scheme != "https") || endpoint.User != nil || endpoint.RawQuery != "" || endpoint.Fragment != "" {
		return fmt.Errorf("batch base URL must be an HTTP(S) endpoint without credentials, query or fragment")
	}
	endpoint.Scheme = strings.ToLower(endpoint.Scheme)
	endpoint.Host = strings.ToLower(endpoint.Host)
	target := formationTarget{BaseURL: strings.TrimRight(endpoint.String(), "/"), SpaceID: client.SpaceID, Shard: client.Shard}
	if strings.TrimSpace(target.SpaceID) == "" || target.Shard < 0 {
		return fmt.Errorf("batch requires a space ID and non-negative shard")
	}
	if c.Target == nil {
		// An old successful checklist cannot establish which endpoint received
		// its files. Pending scan-only entries are safe to bind on first use.
		for _, entry := range c.Entries {
			if entry.Status != batchStatusPending || entry.Attempts != 0 {
				return fmt.Errorf("checklist has no recorded target; use a new --batch-report before submitting")
			}
		}
		c.Target = &target
	} else if *c.Target != target {
		return fmt.Errorf("checklist target mismatch; use a separate --batch-report for this endpoint, space and shard")
	}
	return nil
}

func loadFileFormationChecklist(path, root, selector string) (*fileFormationChecklist, error) {
	data, err := os.ReadFile(path)
	if errors.Is(err, os.ErrNotExist) {
		return &fileFormationChecklist{RootDir: root, Selector: selector, Entries: make(map[string]*fileFormationChecklistEntry)}, nil
	}
	if err != nil {
		return nil, fmt.Errorf("read checklist %q: %w", path, err)
	}
	var checklist fileFormationChecklist
	if err := api.DecodeJSON(data, &checklist); err != nil {
		return nil, fmt.Errorf("parse checklist %q: %w", path, err)
	}
	if checklist.RootDir != root || checklist.Selector != selector {
		return nil, fmt.Errorf("checklist root_dir or selector mismatch")
	}
	if checklist.Entries == nil {
		checklist.Entries = make(map[string]*fileFormationChecklistEntry)
	}
	if err := replayFormationJournal(path+".jsonl", &checklist); err != nil {
		return nil, err
	}
	for key, entry := range checklist.Entries {
		if entry == nil || entry.Path != key {
			return nil, fmt.Errorf("invalid checklist entry %q", key)
		}
		switch entry.Status {
		case batchStatusPending, batchStatusWorking, batchStatusSubmitted, batchStatusSucceeded, batchStatusFailed:
		default:
			return nil, fmt.Errorf("invalid checklist status for %q", key)
		}
	}
	return &checklist, nil
}

func replayFormationJournal(path string, checklist *fileFormationChecklist) error {
	file, err := os.Open(path)
	if errors.Is(err, os.ErrNotExist) {
		return nil
	}
	if err != nil {
		return err
	}
	defer file.Close()
	reader := bufio.NewReader(file)
	for {
		line, err := reader.ReadBytes('\n')
		// A process interruption can leave only the last record incomplete.
		// Its prior working state remains unresolved; it is not auto-retried.
		if err == io.EOF {
			return nil
		}
		if err != nil {
			return err
		}
		var entry fileFormationChecklistEntry
		if err := api.DecodeJSON(line, &entry); err != nil {
			return fmt.Errorf("parse checklist journal: %w", err)
		}
		checklist.Entries[entry.Path] = &entry
	}
}

func saveFileFormationChecklist(path string, checklist *fileFormationChecklist) error {
	checklist.UpdatedAt = time.Now().UTC().Format(time.RFC3339)
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return err
	}
	data, err := json.MarshalIndent(checklist, "", "  ")
	if err != nil {
		return err
	}
	if err := os.WriteFile(path+".tmp", data, 0o600); err != nil {
		return err
	}
	return os.Rename(path+".tmp", path)
}

type formationJournal struct {
	path      string
	file      *os.File
	checklist *fileFormationChecklist
}

func openFormationJournal(path string, checklist *fileFormationChecklist) (*formationJournal, error) {
	// Replaying the same entries after a crash between snapshot and truncate
	// is harmless. The target snapshot is always installed first.
	if err := saveFileFormationChecklist(path, checklist); err != nil {
		return nil, err
	}
	file, err := os.OpenFile(path+".jsonl", os.O_CREATE|os.O_WRONLY|os.O_TRUNC, 0o600)
	if err != nil {
		return nil, err
	}
	return &formationJournal{path: path, file: file, checklist: checklist}, nil
}

func (j *formationJournal) record(entry *fileFormationChecklistEntry) error {
	entry.UpdatedAt = time.Now().UTC().Format(time.RFC3339)
	return json.NewEncoder(j.file).Encode(entry)
}

func (j *formationJournal) close() error {
	if err := j.file.Close(); err != nil {
		return err
	}
	if err := saveFileFormationChecklist(j.path, j.checklist); err != nil {
		return err
	}
	return os.Remove(j.path + ".jsonl")
}

func mergeChecklistEntries(checklist *fileFormationChecklist, root string, files []string) {
	for _, file := range files {
		rel, err := filepath.Rel(root, file)
		if err != nil {
			continue
		}
		if checklist.Entries[rel] == nil {
			checklist.Entries[rel] = &fileFormationChecklistEntry{Path: rel, Status: batchStatusPending}
		}
	}
}

func shouldProcessBatchEntry(entry *fileFormationChecklistEntry, retryFailed bool) bool {
	if entry == nil {
		return true
	}
	switch entry.Status {
	case batchStatusPending:
		return true
	case batchStatusFailed:
		return retryFailed
	default:
		return false
	}
}
