package cmd

import (
	"github.com/ldclabs/anda-brain/anda-cli/api"
	"github.com/spf13/cobra"
)

var recallCmd = &cobra.Command{
	Use:   "recall <query>",
	Short: "Recall memory via natural-language query",
	Long: `Query memory with natural language. The query should describe
what information you want to retrieve from memory.

Example:
  anda-cli recall "What are the user's preferences?"
  anda-cli recall --context-counterparty u1 "What happened in the last meeting?"`,
	Args: cobra.ExactArgs(1),
	RunE: func(cmd *cobra.Command, args []string) error {
		contextCounterparty, _ := cmd.Flags().GetString("context-counterparty")
		if contextCounterparty == "" {
			// Fall back to the deprecated alias.
			contextCounterparty, _ = cmd.Flags().GetString("context-user")
		}
		contextAgent, _ := cmd.Flags().GetString("context-agent")
		contextSource, _ := cmd.Flags().GetString("context-source")
		contextTopic, _ := cmd.Flags().GetString("context-topic")
		structured, _ := cmd.Flags().GetBool("structured")
		budgetEnabled, _ := cmd.Flags().GetBool("budget")

		input := &api.RecallInput{
			Query: args[0],
		}

		ctx := buildInputContext(contextCounterparty, contextAgent, contextSource, contextTopic)
		if ctx != nil {
			input.Context = ctx
		}
		if budgetEnabled || cmd.Flags().Changed("budget-max-tokens") || cmd.Flags().Changed("budget-context-tokens") || cmd.Flags().Changed("budget-tokenizer") {
			budget := &api.RecallBudget{}
			if cmd.Flags().Changed("budget-max-tokens") {
				v, _ := cmd.Flags().GetUint32("budget-max-tokens")
				budget.MaxTokens = &v
			}
			if cmd.Flags().Changed("budget-context-tokens") {
				v, _ := cmd.Flags().GetUint32("budget-context-tokens")
				budget.ContextTokens = &v
			}
			budget.Tokenizer, _ = cmd.Flags().GetString("budget-tokenizer")
			input.Budget = budget
		}

		client := newClient()
		if structured {
			resp, err := client.RecallStructured(cmd.Context(), input)
			if err != nil {
				return err
			}
			return printRPC(cmd, resp)
		}
		resp, err := client.Recall(cmd.Context(), input)
		if err != nil {
			return err
		}
		return printRPC(cmd, resp)
	},
}

func init() {
	recallCmd.Flags().String("context-counterparty", "", "Context counterparty (e.g. user ID)")
	recallCmd.Flags().String("context-user", "", "Context counterparty (deprecated alias)")
	_ = recallCmd.Flags().MarkDeprecated("context-user", "use --context-counterparty instead")
	recallCmd.Flags().String("context-agent", "", "Context agent")
	recallCmd.Flags().String("context-source", "", "Context source")
	recallCmd.Flags().String("context-topic", "", "Context topic")
	recallCmd.Flags().Bool("structured", false, "Return answer, citations, found, and uncertainty")
	recallCmd.Flags().Bool("budget", false, "Return a bounded JSON memory packet using server defaults")
	recallCmd.Flags().Uint32("budget-max-tokens", 0, "Optional packet token limit (1-65536)")
	recallCmd.Flags().Uint32("budget-context-tokens", 0, "Optional cumulative planning-input limit (1-131072)")
	recallCmd.Flags().String("budget-tokenizer", "", "Pinned tokenizer (o200k_base@tiktoken-rs-0.12.0)")
	rootCmd.AddCommand(recallCmd)
}
