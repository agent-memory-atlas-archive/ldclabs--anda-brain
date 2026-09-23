package cmd

import (
	"bytes"
	"context"
	"crypto/sha256"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"

	"github.com/ldclabs/anda-brain/anda-cli/api"
)

func TestBatchTargetContentAndForce(t *testing.T) {
	root := t.TempDir()
	file := filepath.Join(root, "one.md")
	mustWriteFile(t, file, "v1")
	var requests atomic.Int32
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		id := requests.Add(1)
		fmt.Fprintf(w, `{"result":{"content":"","conversation":%d}}`, id)
	}))
	defer server.Close()
	client := api.NewClient(server.URL, "s1", "")
	opts := fileFormationBatchOptions{RootDir: root, Extension: "md", Output: io.Discard}
	run := func() {
		t.Helper()
		if err := runFileFormationBatch(context.Background(), client, opts); err != nil {
			t.Fatal(err)
		}
	}
	run()
	run()
	if requests.Load() != 1 {
		t.Fatal("unchanged file submitted twice")
	}
	mustWriteFile(t, file, "v2")
	run()
	if requests.Load() != 2 {
		t.Fatal("changed file skipped")
	}
	opts.Force = true
	run()
	opts.Force = false
	if requests.Load() != 3 {
		t.Fatal("force did not resubmit")
	}
	for _, other := range []*api.Client{
		api.NewClient(server.URL, "s2", ""), api.NewClient(server.URL+"/other", "s1", ""),
	} {
		if err := runFileFormationBatch(context.Background(), other, opts); err == nil || !strings.Contains(err.Error(), "target mismatch") {
			t.Fatalf("target change allowed: %v", err)
		}
	}
	client.Shard = 2
	if err := runFileFormationBatch(context.Background(), client, opts); err == nil {
		t.Fatal("shard change allowed")
	}
	if requests.Load() != 3 {
		t.Fatal("mismatch made a request")
	}
	report, err := loadFileFormationChecklist(filepath.Join(root, defaultBatchReportFileName), root, "ext:.md")
	if err != nil {
		t.Fatal(err)
	}
	entry := report.Entries["one.md"]
	if report.Target.SpaceID != "s1" || entry.Status != batchStatusSubmitted || entry.Digest == "" || entry.Conversation == nil || *entry.Conversation != 3 {
		t.Fatalf("submission receipt lost: %+v", report)
	}
}

func TestBatchUnresolvedFailuresRemainVisible(t *testing.T) {
	root := t.TempDir()
	mustWriteFile(t, filepath.Join(root, "one.md"), "memory")
	var requests atomic.Int32
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if requests.Add(1) == 1 {
			w.WriteHeader(400)
			_, _ = w.Write([]byte(`{"message":"test failure"}`))
			return
		}
		_, _ = w.Write([]byte(`{"result":{"content":"","conversation":7}}`))
	}))
	defer server.Close()
	var output bytes.Buffer
	opts := fileFormationBatchOptions{RootDir: root, Extension: "md", Output: &output}
	client := api.NewClient(server.URL, "s1", "")
	if err := runFileFormationBatch(context.Background(), client, opts); err == nil {
		t.Fatal("failed batch succeeded")
	}
	output.Reset()
	if err := runFileFormationBatch(context.Background(), client, opts); err == nil {
		t.Fatal("unresolved failure reported success")
	}
	if requests.Load() != 1 || !strings.Contains(output.String(), "unresolved=1") {
		t.Fatalf("failure silently disappeared: %s", output.String())
	}
	opts.RetryFailed = true
	if err := runFileFormationBatch(context.Background(), client, opts); err != nil {
		t.Fatal(err)
	}
	if requests.Load() != 2 {
		t.Fatal("failed entry was not retried")
	}
}

func TestBatchRequiresSuccessfulSubmissionReceipt(t *testing.T) {
	for _, response := range []string{`{}`, `{"error":{"message":"rejected"}}`, `{"result":{"content":"","failed_reason":"failed","conversation":7}}`, `{"result":{"content":""}}`} {
		server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { _, _ = io.WriteString(w, response) }))
		root := t.TempDir()
		mustWriteFile(t, filepath.Join(root, "one.md"), "memory")
		err := runFileFormationBatch(context.Background(), api.NewClient(server.URL, "s1", ""), fileFormationBatchOptions{RootDir: root, Extension: "md", Output: io.Discard})
		server.Close()
		if err == nil {
			t.Fatalf("bad receipt marked submitted: %s", response)
		}
		report, err := loadFileFormationChecklist(filepath.Join(root, defaultBatchReportFileName), root, "ext:.md")
		if err != nil {
			t.Fatal(err)
		}
		if report.Entries["one.md"].Status != batchStatusFailed {
			t.Fatalf("wrong status for %s", response)
		}
	}
}

func TestBatchJournalRecoveryDoesNotResubmitInterruptedWork(t *testing.T) {
	root := t.TempDir()
	file := filepath.Join(root, "one.md")
	mustWriteFile(t, file, "memory")
	var requests atomic.Int32
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		requests.Add(1)
		_, _ = io.WriteString(w, `{"result":{"content":"","conversation":9}}`)
	}))
	defer server.Close()
	client := api.NewClient(server.URL, "s1", "")
	path := filepath.Join(root, defaultBatchReportFileName)
	checklist, err := loadFileFormationChecklist(path, root, "ext:.md")
	if err != nil {
		t.Fatal(err)
	}
	if err := checklist.bindTarget(client); err != nil {
		t.Fatal(err)
	}
	mergeChecklistEntries(checklist, root, []string{file})
	journal, err := openFormationJournal(path, checklist)
	if err != nil {
		t.Fatal(err)
	}
	entry := checklist.Entries["one.md"]
	entry.Status = batchStatusWorking
	entry.Attempts = 1
	entry.Digest = fmt.Sprintf("%x", sha256.Sum256([]byte("memory")))
	if err := journal.record(entry); err != nil {
		t.Fatal(err)
	}
	if _, err := journal.file.WriteString(`{"path":"one.md","status":"submitted"`); err != nil {
		t.Fatal(err)
	}
	if err := journal.file.Close(); err != nil {
		t.Fatal(err)
	} // simulate interruption before final snapshot
	opts := fileFormationBatchOptions{RootDir: root, Extension: "md", RetryFailed: true, Output: io.Discard}
	if err := runFileFormationBatch(context.Background(), client, opts); err == nil {
		t.Fatal("interrupted work was reported complete")
	}
	if requests.Load() != 0 {
		t.Fatal("interrupted submission replayed automatically")
	}
	opts.Force = true
	if err := runFileFormationBatch(context.Background(), client, opts); err != nil {
		t.Fatal(err)
	}
	if requests.Load() != 1 {
		t.Fatal("explicit force did not resubmit")
	}
	if _, err := os.Stat(path + ".jsonl"); !os.IsNotExist(err) {
		t.Fatalf("journal not compacted: %v", err)
	}
}

func TestBatchReplaysCompletedReceipt(t *testing.T) {
	root := t.TempDir()
	file := filepath.Join(root, "one.md")
	mustWriteFile(t, file, "memory")
	var requests atomic.Int32
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		requests.Add(1)
		_, _ = io.WriteString(w, `{"result":{"content":"","conversation":9}}`)
	}))
	defer server.Close()
	client := api.NewClient(server.URL, "s1", "")
	path := filepath.Join(root, defaultBatchReportFileName)
	checklist, err := loadFileFormationChecklist(path, root, "ext:.md")
	if err != nil {
		t.Fatal(err)
	}
	if err := checklist.bindTarget(client); err != nil {
		t.Fatal(err)
	}
	mergeChecklistEntries(checklist, root, []string{file})
	journal, err := openFormationJournal(path, checklist)
	if err != nil {
		t.Fatal(err)
	}
	id := uint64(7)
	entry := checklist.Entries["one.md"]
	entry.Status = batchStatusSubmitted
	entry.Attempts = 1
	entry.Conversation = &id
	entry.Digest = fmt.Sprintf("%x", sha256.Sum256([]byte("memory")))
	if err := journal.record(entry); err != nil {
		t.Fatal(err)
	}
	if err := journal.file.Close(); err != nil {
		t.Fatal(err)
	}
	if err := runFileFormationBatch(context.Background(), client, fileFormationBatchOptions{RootDir: root, Extension: "md", Output: io.Discard}); err != nil {
		t.Fatal(err)
	}
	if requests.Load() != 0 {
		t.Fatal("completed receipt was submitted again")
	}
}

func TestBatchPersistsReceiptBeforeOutputFailure(t *testing.T) {
	root := t.TempDir()
	mustWriteFile(t, filepath.Join(root, "one.md"), "memory")
	var requests atomic.Int32
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		requests.Add(1)
		_, _ = io.WriteString(w, `{"result":{"content":"","conversation":7}}`)
	}))
	defer server.Close()
	client := api.NewClient(server.URL, "s1", "")
	opts := fileFormationBatchOptions{RootDir: root, Extension: "md", Output: failingWriter{}}
	if err := runFileFormationBatch(context.Background(), client, opts); err == nil {
		t.Fatal("output error lost")
	}
	opts.Output = io.Discard
	if err := runFileFormationBatch(context.Background(), client, opts); err != nil {
		t.Fatal(err)
	}
	if requests.Load() != 1 {
		t.Fatal("output failure lost the accepted submission")
	}
}

func TestBatchCannotAdoptUnboundHistoricalSuccess(t *testing.T) {
	report := &fileFormationChecklist{Entries: map[string]*fileFormationChecklistEntry{"one.md": {Path: "one.md", Status: batchStatusSucceeded, Attempts: 1}}}
	if err := report.bindTarget(api.NewClient("http://localhost:8042", "s1", "")); err == nil {
		t.Fatal("unbound history adopted a target")
	}
}

func BenchmarkFormationProgress(b *testing.B) {
	for _, size := range []int{1000, 5000} {
		for _, mode := range []string{"snapshot", "append"} {
			b.Run(fmt.Sprintf("%s/%d", mode, size), func(b *testing.B) {
				path := filepath.Join(b.TempDir(), "report.json")
				checklist := &fileFormationChecklist{RootDir: "/tmp/docs", Selector: "ext:.md", Entries: make(map[string]*fileFormationChecklistEntry, size)}
				for i := 0; i < size; i++ {
					name := fmt.Sprintf("docs/document-%06d.md", i)
					checklist.Entries[name] = &fileFormationChecklistEntry{Path: name, Status: batchStatusSubmitted, Attempts: 1, UpdatedAt: "2026-09-23T00:00:00Z"}
				}
				journal, err := openFormationJournal(path, checklist)
				if err != nil {
					b.Fatal(err)
				}
				b.Cleanup(func() {
					if err := journal.close(); err != nil {
						b.Error(err)
					}
				})
				entry := checklist.Entries["docs/document-000000.md"]
				b.ReportAllocs()
				b.ResetTimer()
				for b.Loop() {
					var err error
					if mode == "snapshot" {
						err = saveFileFormationChecklist(path, checklist)
					} else {
						err = journal.record(entry)
					}
					if err != nil {
						b.Fatal(err)
					}
				}
			})
		}
	}
}
