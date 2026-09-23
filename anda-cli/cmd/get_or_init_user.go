package cmd

import (
	"github.com/ldclabs/anda-brain/anda-cli/api"
	"github.com/spf13/cobra"
)

var getOrInitUserCmd = &cobra.Command{
	Use:   "get-or-init-user <user>",
	Short: "Get or initialize a user concept",
	Long: `Get or initialize a user concept node in the space.

The HTTP endpoint returns an RPC envelope containing the Concept.

Example:
  anda-cli --space-id my_space --token $TOKEN get-or-init-user principal_123
  anda-cli --space-id my_space --token $TOKEN get-or-init-user principal_123 --name Alice`,
	Args: cobra.ExactArgs(1),
	RunE: func(cmd *cobra.Command, args []string) error {
		name, _ := cmd.Flags().GetString("name")

		input := &api.GetOrInitUserInput{
			User: args[0],
		}
		if name != "" {
			input.Name = &name
		}

		client := newClient()
		resp, err := client.GetOrInitUser(cmd.Context(), input)
		if err != nil {
			return err
		}
		return printRPC(cmd, resp)
	},
}

func init() {
	getOrInitUserCmd.Flags().String("name", "", "Optional display name used when creating the user concept")
	rootCmd.AddCommand(getOrInitUserCmd)
}
