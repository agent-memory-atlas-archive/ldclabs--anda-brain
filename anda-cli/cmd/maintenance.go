package cmd

import (
	"time"

	"github.com/ldclabs/anda-brain/anda-cli/api"
	"github.com/spf13/cobra"
)

var maintenanceCmd = &cobra.Command{
	Use:   "maintenance",
	Short: "Trigger maintenance (sleep/consolidation)",
	Long: `Trigger a maintenance task for memory consolidation.

Example:
  anda-cli maintenance
  anda-cli maintenance --trigger on_demand --scope full`,
	Args: cobra.NoArgs,
	RunE: func(cmd *cobra.Command, args []string) error {
		input := &api.MaintenanceInput{
			Timestamp: time.Now().UTC().Format("2006-01-02T15:04:05.000Z"),
		}
		input.Trigger, _ = cmd.Flags().GetString("trigger")
		input.Scope, _ = cmd.Flags().GetString("scope")

		var parameters api.MaintenanceParameters
		set := false
		intParameter := func(name string, field **int) {
			if cmd.Flags().Changed(name) {
				value, _ := cmd.Flags().GetInt(name)
				*field = &value
				set = true
			}
		}
		intParameter("stale-event-threshold-days", &parameters.StaleEventThresholdDays)
		intParameter("unconsolidated-max-backlog", &parameters.UnconsolidatedMaxBacklog)
		intParameter("orphan-max-count", &parameters.OrphanMaxCount)
		if set {
			input.Parameters = &parameters
		}

		resp, err := newClient().Maintenance(cmd.Context(), input)
		if err != nil {
			return err
		}
		return printRPC(cmd, resp)
	},
}

func init() {
	maintenanceCmd.Flags().String("trigger", "", "Trigger type: scheduled, threshold, on_demand (default: on_demand)")
	maintenanceCmd.Flags().String("scope", "", "Scope: full, quick, daydream (default: daydream)")
	maintenanceCmd.Flags().Int("stale-event-threshold-days", 0, "Per-run stale event threshold (1-365)")
	maintenanceCmd.Flags().Int("unconsolidated-max-backlog", 0, "Per-run unconsolidated backlog limit (1-10000)")
	maintenanceCmd.Flags().Int("orphan-max-count", 0, "Per-run orphan limit (1-10000)")
	rootCmd.AddCommand(maintenanceCmd)
}
