package cmd

import (
	"fmt"
	"strconv"

	"github.com/spf13/cobra"
)

var conversationsCmd = &cobra.Command{
	Use:   "conversations",
	Short: "Manage conversations",
}

var listConversationsCmd = &cobra.Command{
	Use:   "list",
	Short: "List conversations with pagination",
	Args:  cobra.NoArgs,
	RunE: func(cmd *cobra.Command, args []string) error {
		cursor, _ := cmd.Flags().GetString("cursor")
		limit, _ := cmd.Flags().GetInt("limit")
		if limit < 0 {
			return fmt.Errorf("--limit must be non-negative")
		}
		collection, _ := cmd.Flags().GetString("collection")

		client := newClient()
		resp, err := client.ListConversations(cmd.Context(), cursor, limit, collection)
		if err != nil {
			return err
		}
		return printRPC(cmd, resp)
	},
}

var getConversationCmd = &cobra.Command{
	Use:   "get <conversation_id>",
	Short: "Get a single conversation detail",
	Args:  cobra.ExactArgs(1),
	RunE: func(cmd *cobra.Command, args []string) error {
		id, err := strconv.ParseUint(args[0], 10, 64)
		if err != nil {
			return fmt.Errorf("invalid conversation ID: %w", err)
		}

		collection, _ := cmd.Flags().GetString("collection")

		client := newClient()
		resp, err := client.GetConversation(cmd.Context(), id, collection)
		if err != nil {
			return err
		}
		return printRPC(cmd, resp)
	},
}

var getConversationDeltaCmd = &cobra.Command{
	Use:   "delta <conversation_id>",
	Short: "Get incremental conversation updates",
	Args:  cobra.ExactArgs(1),
	RunE: func(cmd *cobra.Command, args []string) error {
		id, err := strconv.ParseUint(args[0], 10, 64)
		if err != nil {
			return fmt.Errorf("invalid conversation ID: %w", err)
		}

		messagesOffset, _ := cmd.Flags().GetInt("messages-offset")
		artifactsOffset, _ := cmd.Flags().GetInt("artifacts-offset")
		if messagesOffset < 0 || artifactsOffset < 0 {
			return fmt.Errorf("offsets must be non-negative")
		}

		collection, _ := cmd.Flags().GetString("collection")

		client := newClient()
		resp, err := client.GetConversationDelta(cmd.Context(), id, messagesOffset, artifactsOffset, collection)
		if err != nil {
			return err
		}
		return printRPC(cmd, resp)
	},
}

func init() {
	listConversationsCmd.Flags().String("cursor", "", "Pagination cursor")
	listConversationsCmd.Flags().Int("limit", 0, "Number of conversations to return")
	listConversationsCmd.Flags().String("collection", "", "Collection name, empty, 'recall', or 'maintenance'")

	getConversationCmd.Flags().String("collection", "", "Collection name, empty, 'recall', or 'maintenance'")
	getConversationDeltaCmd.Flags().String("collection", "", "Collection name, empty, 'recall', or 'maintenance'")
	getConversationDeltaCmd.Flags().Int("messages-offset", 0, "Skip this many existing messages")
	getConversationDeltaCmd.Flags().Int("artifacts-offset", 0, "Skip this many existing artifacts")

	conversationsCmd.AddCommand(listConversationsCmd)
	conversationsCmd.AddCommand(getConversationCmd)
	conversationsCmd.AddCommand(getConversationDeltaCmd)
	rootCmd.AddCommand(conversationsCmd)
}
