package cmd

import (
	"context"
	"crypto/sha256"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/ldclabs/anda-brain/anda-cli/api"
)

type fileFormationBatchOptions struct {
	RootDir      string
	FileName     string
	Extension    string
	ReportPath   string
	RetryFailed  bool
	DryRun       bool
	Force        bool
	InputContext *api.InputContext
	Output       io.Writer
}

func runFileFormationBatch(ctx context.Context, client *api.Client, opts fileFormationBatchOptions) (runErr error) {
	if strings.TrimSpace(opts.RootDir) == "" {
		return fmt.Errorf("--batch-dir cannot be empty")
	}
	if client == nil && !opts.DryRun {
		return fmt.Errorf("batch client is required")
	}
	if opts.Output == nil {
		opts.Output = os.Stdout
	}
	logf := func(format string, args ...any) error {
		_, err := fmt.Fprintf(opts.Output, format, args...)
		return err
	}
	absRootDir, err := filepath.Abs(opts.RootDir)
	if err != nil {
		return fmt.Errorf("resolve batch dir: %w", err)
	}
	selector, err := resolveBatchSelector(opts.FileName, opts.Extension)
	if err != nil {
		return err
	}
	reportPath, err := resolveBatchReportPath(absRootDir, opts.ReportPath)
	if err != nil {
		return err
	}
	files, err := findBatchFiles(absRootDir, selector, map[string]bool{
		reportPath: true, reportPath + ".tmp": true, reportPath + ".jsonl": true,
	})
	if err != nil {
		return err
	}
	if len(files) == 0 {
		return fmt.Errorf("no files matched selector %q under %q", selector, absRootDir)
	}
	checklist, err := loadFileFormationChecklist(reportPath, absRootDir, selector)
	if err != nil {
		return err
	}
	if client != nil {
		if err := checklist.bindTarget(client); err != nil {
			return err
		}
	}
	mergeChecklistEntries(checklist, absRootDir, files)

	// A snapshot binds the target before any request. Only the changed entry
	// is appended during submission; the final snapshot compacts that log.
	var journal *formationJournal
	if !opts.DryRun {
		journal, err = openFormationJournal(reportPath, checklist)
		if err != nil {
			return err
		}
		defer func() { runErr = errors.Join(runErr, journal.close()) }()
	}
	submitted, failed, unresolved, skipped, wouldSubmit := 0, 0, 0, 0, 0
	for idx, file := range files {
		if err := ctx.Err(); err != nil {
			return err
		}
		rel, err := filepath.Rel(absRootDir, file)
		if err != nil {
			return err
		}
		entry := checklist.Entries[rel]
		content, readErr := os.ReadFile(file)
		digest := ""
		if readErr == nil {
			digest = fmt.Sprintf("%x", sha256.Sum256(content))
			if entry.Digest != "" && entry.Digest != digest {
				entry.Status = batchStatusPending
				entry.Conversation = nil
			}
			if !opts.Force && !shouldProcessBatchEntry(entry, opts.RetryFailed) {
				skipped++
				if entry.Status == batchStatusFailed || entry.Status == batchStatusWorking {
					unresolved++
				}
				if err := logf("[%d/%d] Skip %s (status=%s)\n", idx+1, len(files), rel, entry.Status); err != nil {
					return err
				}
				continue
			}
		}
		if opts.DryRun {
			if readErr != nil {
				return fmt.Errorf("read %s: %w", rel, readErr)
			}
			wouldSubmit++
			if err := logf("[%d/%d] DRY  %s\n", idx+1, len(files), rel); err != nil {
				return err
			}
			continue
		}

		entry.Attempts++
		entry.Digest = digest
		entry.Status = batchStatusWorking
		entry.LastError = ""
		entry.Conversation = nil
		if err := journal.record(entry); err != nil {
			return err
		}
		var output *api.AgentOutput
		submitErr := readErr
		if submitErr == nil {
			output, submitErr = submitFormationFile(ctx, client, file, content, opts.InputContext)
		}
		if submitErr != nil {
			entry.Status = batchStatusFailed
			entry.LastError = submitErr.Error()
			failed++
		} else {
			entry.Status = batchStatusSubmitted
			entry.Conversation = output.Conversation
			submitted++
		}
		// Persist the response before writing progress to a possibly closed pipe.
		if err := journal.record(entry); err != nil {
			return err
		}
		if submitErr != nil {
			if err := logf("[%d/%d] Fail %s: %v\n", idx+1, len(files), rel, submitErr); err != nil {
				return err
			}
		} else {
			if err := logf("[%d/%d] Submitted %s (conversation=%d)\n", idx+1, len(files), rel, *entry.Conversation); err != nil {
				return err
			}
		}
	}
	if opts.DryRun {
		return logf("Batch dry-run done. total=%d would_submit=%d skipped=%d unresolved=%d\n", len(files), wouldSubmit, skipped, unresolved)
	}
	if err := logf("Batch done. total=%d submitted=%d failed=%d unresolved=%d skipped=%d checklist=%s\n", len(files), submitted, failed, unresolved, skipped, reportPath); err != nil {
		return err
	}
	if failed+unresolved > 0 {
		return fmt.Errorf("batch has %d unresolved submissions; see checklist %q", failed+unresolved, reportPath)
	}
	return nil
}

func submitFormationFile(ctx context.Context, client *api.Client, file string, content []byte, inputContext *api.InputContext) (*api.AgentOutput, error) {
	messages, err := parseMessagesInput(string(content))
	if err != nil {
		return nil, fmt.Errorf("parse messages: %w", err)
	}
	source := api.InputContext{}
	if inputContext != nil {
		source = *inputContext
	}
	if source.Source == "" {
		source.Source = file
	}
	response, err := client.Formation(ctx, &api.FormationInput{
		Messages: messages, Context: &source, Timestamp: time.Now().UTC().Format(time.RFC3339),
	})
	if err != nil {
		return nil, err
	}
	if response.Error != nil {
		return nil, response.Error
	}
	if response.Result == nil {
		return nil, fmt.Errorf("formation returned no result")
	}
	if err := response.Result.Failure(); err != nil {
		return nil, err
	}
	if response.Result.Conversation == nil {
		return nil, fmt.Errorf("formation returned no conversation ID")
	}
	return response.Result, nil
}

func resolveBatchSelector(fileName, extension string) (string, error) {
	fileName = strings.TrimSpace(fileName)
	extension = strings.TrimSpace(extension)

	if fileName == "" && extension == "" {
		return "", fmt.Errorf("batch selector is required: set --batch-file-name or --batch-ext")
	}
	if fileName != "" && extension != "" {
		return "", fmt.Errorf("--batch-file-name and --batch-ext cannot be used together")
	}

	if fileName != "" {
		return "name:" + strings.ToLower(fileName), nil
	}

	if !strings.HasPrefix(extension, ".") {
		extension = "." + extension
	}
	if extension == "." {
		return "", fmt.Errorf("invalid --batch-ext value")
	}
	return "ext:" + strings.ToLower(extension), nil
}

func resolveBatchReportPath(rootDir, reportPath string) (string, error) {
	if strings.TrimSpace(reportPath) == "" {
		return filepath.Join(rootDir, defaultBatchReportFileName), nil
	}

	absReportPath, err := filepath.Abs(reportPath)
	if err != nil {
		return "", fmt.Errorf("resolve batch report path: %w", err)
	}
	return absReportPath, nil
}

// findBatchFiles walks rootDir collecting files that match the selector.
// Hidden entries (dot-prefixed, e.g. .git, .DS_Store) and excludePaths
// (the batch checklist and its temp file) are skipped so bookkeeping and
// VCS internals are never submitted as memory content.
func findBatchFiles(rootDir, selector string, excludePaths map[string]bool) ([]string, error) {
	files := make([]string, 0)
	err := filepath.WalkDir(rootDir, func(path string, d fs.DirEntry, walkErr error) error {
		if walkErr != nil {
			return walkErr
		}
		if path != rootDir && strings.HasPrefix(d.Name(), ".") {
			if d.IsDir() {
				return filepath.SkipDir
			}
			return nil
		}
		if d.IsDir() || !d.Type().IsRegular() {
			return nil
		}
		if excludePaths[path] {
			return nil
		}
		if matchesBatchSelector(d.Name(), selector) {
			files = append(files, path)
		}
		return nil
	})
	if err != nil {
		return nil, fmt.Errorf("scan batch dir %q: %w", rootDir, err)
	}
	return files, nil
}

func matchesBatchSelector(fileName, selector string) bool {
	if strings.HasPrefix(selector, "name:") {
		name := strings.TrimPrefix(selector, "name:")
		return strings.EqualFold(fileName, name)
	}
	if strings.HasPrefix(selector, "ext:") {
		ext := strings.TrimPrefix(selector, "ext:")
		return strings.EqualFold(filepath.Ext(fileName), ext)
	}
	return false
}
