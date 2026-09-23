package cmd

import (
	"github.com/spf13/cobra"
)

var infoCmd = &cobra.Command{
	Use:   "info",
	Short: "Get space information",
	Args:  cobra.NoArgs,
	RunE: func(cmd *cobra.Command, args []string) error {
		client := newClient()
		resp, err := client.GetSpaceInfo(cmd.Context())
		if err != nil {
			return err
		}
		return printRPC(cmd, resp)
	},
}

var formationStatusCmd = &cobra.Command{
	Use:   "formation-status",
	Short: "Get formation processing status",
	Args:  cobra.NoArgs,
	RunE: func(cmd *cobra.Command, args []string) error {
		client := newClient()
		resp, err := client.GetFormationStatus(cmd.Context())
		if err != nil {
			return err
		}
		return printRPC(cmd, resp)
	},
}

func init() {
	rootCmd.AddCommand(infoCmd)
	rootCmd.AddCommand(formationStatusCmd)
}
