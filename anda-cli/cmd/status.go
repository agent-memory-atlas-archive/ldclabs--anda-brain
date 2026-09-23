package cmd

import (
	"github.com/spf13/cobra"
)

var statusCmd = &cobra.Command{
	Use:   "status",
	Short: "Get service information (name, version, sharding)",
	Args:  cobra.NoArgs,
	RunE: func(cmd *cobra.Command, args []string) error {
		client := newClient()
		info, err := client.GetInfo(cmd.Context())
		if err != nil {
			return err
		}
		return printJSON(cmd, info)
	},
}

func init() {
	rootCmd.AddCommand(statusCmd)
}
