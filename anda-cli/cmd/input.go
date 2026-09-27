package cmd

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"strings"

	"github.com/ldclabs/anda-brain/anda-cli/api"
	"github.com/spf13/cobra"
)

// readAtFile returns the contents of the file an "@path" input names, and
// any other input unchanged. fromFile reports which one it was.
func readAtFile(input string) (value string, fromFile bool, err error) {
	path, ok := strings.CutPrefix(input, "@")
	if !ok {
		return input, false, nil
	}
	path = strings.TrimSpace(path)
	if path == "" {
		return "", true, fmt.Errorf("empty file path after '@'")
	}
	data, err := os.ReadFile(path)
	if err != nil {
		return "", true, fmt.Errorf("read file %q: %w", path, err)
	}
	return string(data), true, nil
}

// readInput returns the inline flag's value, the contents of the file flag,
// or piped stdin. At most one of the two flags may be set.
func readInput(cmd *cobra.Command, inlineFlag, fileFlag string) (string, error) {
	inline, _ := cmd.Flags().GetString(inlineFlag)
	file, _ := cmd.Flags().GetString(fileFlag)
	switch {
	case inline != "" && file != "":
		return "", fmt.Errorf("--%s and --%s cannot be used together", inlineFlag, fileFlag)
	case inline != "":
		return inline, nil
	case file != "":
		data, err := os.ReadFile(file)
		if err != nil {
			return "", fmt.Errorf("read file %q: %w", file, err)
		}
		return string(data), nil
	}
	in := cmd.InOrStdin()
	if stdin, ok := in.(*os.File); ok {
		stat, err := stdin.Stat()
		if err != nil {
			return "", fmt.Errorf("inspect stdin: %w", err)
		}
		if stat.Mode()&os.ModeCharDevice != 0 {
			return "", fmt.Errorf("--%s or --%s is required, or pipe input via stdin", inlineFlag, fileFlag)
		}
	}
	data, err := io.ReadAll(in)
	if err != nil {
		return "", fmt.Errorf("read stdin: %w", err)
	}
	return string(data), nil
}

// resolveSecretInput resolves a secret flag value: a literal value is
// returned as-is, while an "@path/to/file" input is replaced by the trimmed
// contents of that file. This keeps secrets out of shell history and process
// listings.
func resolveSecretInput(input string) (string, error) {
	value, fromFile, err := readAtFile(strings.TrimSpace(input))
	if err != nil {
		return "", err
	}
	value = strings.TrimSpace(value)
	if fromFile && value == "" {
		return "", fmt.Errorf("secret file %s is empty", strings.TrimSpace(input))
	}
	return value, nil
}

// readJSONObject accepts inline JSON or @file and preserves omitted fields.
func readJSONObject[T any](input string) (T, error) {
	var value T
	input, _, err := readAtFile(input)
	if err != nil {
		return value, err
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
