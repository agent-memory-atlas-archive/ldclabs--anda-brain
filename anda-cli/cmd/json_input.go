package cmd

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"strings"

	"github.com/ldclabs/anda-brain/anda-cli/api"
)

// readJSONObject accepts inline JSON or @file and preserves omitted fields.
func readJSONObject[T any](input string) (T, error) {
	var value T
	if strings.HasPrefix(input, "@") {
		path := strings.TrimPrefix(input, "@")
		if path == "" {
			return value, fmt.Errorf("empty JSON file path")
		}
		data, err := os.ReadFile(path)
		if err != nil {
			return value, fmt.Errorf("read JSON file %q: %w", path, err)
		}
		input = string(data)
	}
	trimmed := strings.TrimSpace(input)
	if !strings.HasPrefix(trimmed, "{") {
		return value, fmt.Errorf("expected a JSON object or @file")
	}
	decoder := json.NewDecoder(bytes.NewBufferString(trimmed))
	decoder.DisallowUnknownFields()
	decoder.UseNumber()
	if err := decoder.Decode(&value); err != nil {
		return value, fmt.Errorf("invalid JSON object: %w", err)
	}
	var trailing any
	if err := decoder.Decode(&trailing); err != io.EOF {
		return value, fmt.Errorf("JSON input must contain one object")
	}
	return value, nil
}

// Exports carry a docs summary in addition to the importable bundle. Accept
// that one known field while continuing to reject misspelled input fields.
func readWikiImport(input string) (api.WikiImportInput, error) {
	value, err := readJSONObject[struct {
		api.WikiImportInput
		Docs *uint64 `json:"docs,omitempty"`
	}](input)
	return value.WikiImportInput, err
}
