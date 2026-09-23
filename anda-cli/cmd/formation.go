package cmd

import (
	"encoding/json"
	"fmt"
	"io"
	"os"
	"strings"
	"time"

	"github.com/ldclabs/anda-brain/anda-cli/api"
	"github.com/spf13/cobra"
)

var formationCmd = &cobra.Command{
	Use:   "formation",
	Short: "Submit a memory formation task",
	Long: `Submit conversation messages for memory encoding.

Messages are provided via --messages or stdin.
Input can be a JSON message array/object, or plain text.
Plain text is treated as one message: role="user", content=<text>.

Example:
  anda-cli formation --messages '[{"role":"user","content":"Hello"},{"role":"assistant","content":"Hi there!"}]'
  anda-cli formation --file ./message.txt
  echo '[{"role":"user","content":"Hello"}]' | anda-cli formation`,
	Args: cobra.NoArgs,
	RunE: func(cmd *cobra.Command, args []string) error {
		messagesJSON, _ := cmd.Flags().GetString("messages")
		messagesFile, _ := cmd.Flags().GetString("file")
		batchDir, _ := cmd.Flags().GetString("batch-dir")
		batchFileName, _ := cmd.Flags().GetString("batch-file-name")
		batchExt, _ := cmd.Flags().GetString("batch-ext")
		batchReport, _ := cmd.Flags().GetString("batch-report")
		batchRetryFailed, _ := cmd.Flags().GetBool("batch-retry-failed")
		batchDryRun, _ := cmd.Flags().GetBool("batch-dry-run")
		batchForce, _ := cmd.Flags().GetBool("batch-force")
		contextUser, _ := cmd.Flags().GetString("context-counterparty")
		contextAgent, _ := cmd.Flags().GetString("context-agent")
		contextSource, _ := cmd.Flags().GetString("context-source")
		contextTopic, _ := cmd.Flags().GetString("context-topic")

		ctx := buildInputContext(contextUser, contextAgent, contextSource, contextTopic)

		if batchDir != "" {
			if messagesJSON != "" || messagesFile != "" {
				return fmt.Errorf("--batch-dir cannot be used with --messages or --file")
			}

			client := newClient()
			err := runFileFormationBatch(cmd.Context(), client, fileFormationBatchOptions{
				RootDir:      batchDir,
				FileName:     batchFileName,
				Extension:    batchExt,
				ReportPath:   batchReport,
				RetryFailed:  batchRetryFailed,
				DryRun:       batchDryRun,
				Force:        batchForce,
				Output:       cmd.OutOrStdout(),
				InputContext: ctx,
			})
			if err != nil {
				return err
			}
			return nil
		}

		for _, name := range []string{"batch-file-name", "batch-ext", "batch-report", "batch-retry-failed", "batch-dry-run", "batch-force"} {
			if cmd.Flags().Changed(name) {
				return fmt.Errorf("--%s requires --batch-dir", name)
			}
		}

		var messages []api.Message

		if messagesJSON != "" && messagesFile != "" {
			return fmt.Errorf("--messages and --file cannot be used together")
		}

		if messagesJSON != "" {
			var err error
			messages, err = parseMessagesInput(messagesJSON)
			if err != nil {
				return fmt.Errorf("parse messages input: %w", err)
			}
		} else if messagesFile != "" {
			data, err := os.ReadFile(messagesFile)
			if err != nil {
				return fmt.Errorf("read file %q: %w", messagesFile, err)
			}
			messages, err = parseMessagesInput(string(data))
			if err != nil {
				return fmt.Errorf("parse file input: %w", err)
			}

			if ctx == nil {
				ctx = &api.InputContext{Source: messagesFile}
			} else if ctx.Source == "" {
				ctx.Source = messagesFile
			}
		} else {
			stat, err := os.Stdin.Stat()
			if err != nil {
				return fmt.Errorf("inspect stdin: %w", err)
			}
			if (stat.Mode() & os.ModeCharDevice) == 0 {
				data, err := io.ReadAll(os.Stdin)
				if err != nil {
					return fmt.Errorf("read stdin: %w", err)
				}
				messages, err = parseMessagesInput(string(data))
				if err != nil {
					return fmt.Errorf("parse stdin messages: %w", err)
				}
			} else {
				return fmt.Errorf("--messages or --file is required, or pipe input via stdin")
			}
		}

		input := &api.FormationInput{
			Messages:  messages,
			Timestamp: time.Now().UTC().Format(time.RFC3339),
		}

		if ctx != nil {
			input.Context = ctx
		}

		client := newClient()
		resp, err := client.Formation(cmd.Context(), input)
		if err != nil {
			return err
		}
		return printRPC(cmd, resp)
	},
}

func parseMessagesInput(raw string) ([]api.Message, error) {
	raw = strings.TrimSpace(raw)
	if raw == "" || raw == "null" {
		return nil, fmt.Errorf("empty input")
	}

	// A valid JSON array/object must be a valid message payload: falling back to
	// plain text would silently submit malformed structures as memory content.
	if json.Valid([]byte(raw)) {
		switch raw[0] {
		case '[':
			var messages []api.Message
			if err := json.Unmarshal([]byte(raw), &messages); err != nil {
				return nil, fmt.Errorf("invalid message array: %w", err)
			}
			if len(messages) == 0 {
				return nil, fmt.Errorf("messages cannot be empty")
			}
			if err := validateMessages(messages); err != nil {
				return nil, err
			}
			return messages, nil
		case '{':
			var single api.Message
			if err := json.Unmarshal([]byte(raw), &single); err != nil {
				return nil, fmt.Errorf("invalid message object: %w", err)
			}
			messages := []api.Message{single}
			if err := validateMessages(messages); err != nil {
				return nil, err
			}
			return messages, nil
		}
	}

	return []api.Message{{
		Role:    "user",
		Content: api.MessageContentFromText(raw),
	}}, nil
}

func validateMessages(messages []api.Message) error {
	for idx, message := range messages {
		if strings.TrimSpace(string(message.Role)) == "" {
			return fmt.Errorf("message[%d] is not a valid message: missing role", idx)
		}
		if len(message.Content) == 0 {
			return fmt.Errorf("message[%d] is not a valid message: missing content", idx)
		}
	}
	return nil
}

// buildInputContext returns nil when every field is empty so the request
// omits the context instead of sending an empty object.
func buildInputContext(user, agent, source, topic string) *api.InputContext {
	if user == "" && agent == "" && source == "" && topic == "" {
		return nil
	}
	return &api.InputContext{
		Counterparty: user,
		Agent:        agent,
		Source:       source,
		Topic:        topic,
	}
}

func init() {
	formationCmd.Flags().String("messages", "", "Messages as JSON or plain text")
	formationCmd.Flags().String("file", "", "Read messages from file (JSON or plain text)")
	formationCmd.Flags().String("batch-dir", "", "Recursively submit files under the given directory")
	formationCmd.Flags().String("batch-file-name", "", "Submit files with exact filename match (case-insensitive), e.g. Skill.md")
	formationCmd.Flags().String("batch-ext", "", "Submit files by extension, e.g. .md or md")
	formationCmd.Flags().String("batch-report", "", "Batch checklist JSON path (default: <batch-dir>/.formation-batch-checklist.json)")
	formationCmd.Flags().Bool("batch-force", false, "Explicitly resubmit matched files, including unchanged submissions")
	formationCmd.Flags().Bool("batch-retry-failed", false, "Retry files previously marked as failed in checklist")
	formationCmd.Flags().Bool("batch-dry-run", false, "Dry run: scan and report matched files without submitting formation")
	formationCmd.Flags().String("context-counterparty", "", "Context counterparty (e.g. user ID)")
	formationCmd.Flags().String("context-agent", "", "Context agent")
	formationCmd.Flags().String("context-source", "", "Context source")
	formationCmd.Flags().String("context-topic", "", "Context topic")
	rootCmd.AddCommand(formationCmd)
}
