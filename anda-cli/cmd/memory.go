package cmd

import (
	"fmt"

	"github.com/ldclabs/anda-brain/anda-cli/api"
	"github.com/spf13/cobra"
)

var probeCmd = &cobra.Command{
	Use:   "probe <query>",
	Short: "Check memory retrieval reachability without a model call",
	Args:  cobra.ExactArgs(1),
	RunE: func(cmd *cobra.Command, args []string) error {
		input := &api.ProbeInput{Query: args[0]}
		if cmd.Flags().Changed("limit") {
			limit, _ := cmd.Flags().GetInt("limit")
			input.Limit = &limit
		}
		response, err := newClient().Probe(cmd.Context(), input)
		if err != nil {
			return err
		}
		return printRPC(cmd, response)
	},
}

var memoryStatusCmd = &cobra.Command{
	Use:   "memory-status",
	Short: "Get memory metrics and latest maintenance reports",
	Args:  cobra.NoArgs,
	RunE: func(cmd *cobra.Command, args []string) error {
		response, err := newClient().GetMemoryStatus(cmd.Context())
		if err != nil {
			return err
		}
		return printRPC(cmd, response)
	},
}

var memoryCmd = &cobra.Command{Use: "memory", Short: "Memory Interface intents, and pin or forget graph memory entities"}

var memoryPinCmd = &cobra.Command{
	Use:   "pin <entity>",
	Short: "Pin or unpin a graph entity",
	Args:  cobra.ExactArgs(1),
	RunE: func(cmd *cobra.Command, args []string) error {
		pinned, _ := cmd.Flags().GetBool("pinned")
		response, err := newClient().PinMemory(cmd.Context(), &api.MemoryPinInput{Entity: args[0], Pinned: &pinned})
		if err != nil {
			return err
		}
		return printRPC(cmd, response)
	},
}

var memoryForgetCmd = &cobra.Command{
	Use:   "forget <entity> [entity...]",
	Short: "Delete graph entities, or with --mode run a Memory Interface forget",
	Args:  cobra.MinimumNArgs(1),
	RunE: func(cmd *cobra.Command, args []string) error {
		if mode, _ := cmd.Flags().GetString("mode"); mode != "" {
			if dryRun, _ := cmd.Flags().GetBool("dry-run"); dryRun {
				return fmt.Errorf("--dry-run is not supported with --mode; no deletion was submitted")
			}
			if len(args) != 1 {
				return fmt.Errorf("a Memory Interface forget names one target")
			}
			return memoryInterfaceForget(cmd, args[0], mode)
		}
		dryRun, _ := cmd.Flags().GetBool("dry-run")
		response, err := newClient().ForgetMemory(cmd.Context(), &api.MemoryForgetInput{Entities: args, DryRun: dryRun})
		if err != nil {
			return err
		}
		return printRPC(cmd, response)
	},
}

var shadowEvalCmd = &cobra.Command{
	Use:   "shadow-eval",
	Short: "Compare a candidate memory policy on forked copies",
	Args:  cobra.NoArgs,
	RunE: func(cmd *cobra.Command, args []string) error {
		policyArg, _ := cmd.Flags().GetString("policy")
		if policyArg == "" {
			return fmt.Errorf("--policy is required")
		}
		policy, err := readJSONObject[api.MemoryPolicy](policyArg)
		if err != nil {
			return fmt.Errorf("--policy: %w", err)
		}
		input := &api.ShadowEvalInput{Policy: policy}
		if cmd.Flags().Changed("replay-sample") {
			sample, _ := cmd.Flags().GetInt("replay-sample")
			input.ReplaySample = &sample
		}
		response, err := newClient().ShadowEval(cmd.Context(), input)
		if err != nil {
			return err
		}
		return printRPC(cmd, response)
	},
}

func init() {
	probeCmd.Flags().Int("limit", 0, "Maximum hits (server default 8)")
	memoryPinCmd.Flags().Bool("pinned", true, "Set false to unpin")
	memoryForgetCmd.Flags().Bool("dry-run", false, "Report deletions without applying them")
	memoryCmd.AddCommand(memoryPinCmd, memoryForgetCmd)
	shadowEvalCmd.Flags().String("policy", "", "Candidate memory policy as inline JSON or @file")
	shadowEvalCmd.Flags().Int("replay-sample", 0, "Number of recent recalls to replay (1-16)")
	managementCmd.AddCommand(shadowEvalCmd)
	rootCmd.AddCommand(probeCmd, memoryStatusCmd, memoryCmd)
}
