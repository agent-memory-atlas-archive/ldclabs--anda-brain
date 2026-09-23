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
		trigger, _ := cmd.Flags().GetString("trigger")
		scope, _ := cmd.Flags().GetString("scope")

		input := &api.MaintenanceInput{
			Timestamp: time.Now().UTC().Format(time.RFC3339),
		}
		if trigger != "" {
			input.Trigger = trigger
		}
		if scope != "" {
			input.Scope = scope
		}
		parameters := &api.MaintenanceParameters{}
		if cmd.Flags().Changed("stale-event-threshold-days") {
			v, _ := cmd.Flags().GetInt("stale-event-threshold-days")
			parameters.StaleEventThresholdDays = &v
		}
		if cmd.Flags().Changed("memory-strength-decay-factor") {
			v, _ := cmd.Flags().GetFloat64("memory-strength-decay-factor")
			parameters.MemoryStrengthDecayFactor = &v
		}
		if cmd.Flags().Changed("unconsolidated-max-backlog") {
			v, _ := cmd.Flags().GetInt("unconsolidated-max-backlog")
			parameters.UnconsolidatedMaxBacklog = &v
		}
		if cmd.Flags().Changed("orphan-max-count") {
			v, _ := cmd.Flags().GetInt("orphan-max-count")
			parameters.OrphanMaxCount = &v
		}
		if cmd.Flags().Changed("stale-event-threshold-days") || cmd.Flags().Changed("memory-strength-decay-factor") || cmd.Flags().Changed("unconsolidated-max-backlog") || cmd.Flags().Changed("orphan-max-count") {
			input.Parameters = parameters
		}

		client := newClient()
		resp, err := client.Maintenance(cmd.Context(), input)
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
	maintenanceCmd.Flags().Float64("memory-strength-decay-factor", 0, "Per-run accessibility decay factor (0,1]")
	maintenanceCmd.Flags().Int("unconsolidated-max-backlog", 0, "Per-run unconsolidated backlog limit (1-10000)")
	maintenanceCmd.Flags().Int("orphan-max-count", 0, "Per-run orphan limit (1-10000)")
	rootCmd.AddCommand(maintenanceCmd)
}
