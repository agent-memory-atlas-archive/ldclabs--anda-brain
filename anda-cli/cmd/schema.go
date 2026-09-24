package cmd

import (
	"fmt"

	"github.com/ldclabs/anda-brain/anda-cli/api"
	"github.com/spf13/cobra"
)

var schemaCmd = &cobra.Command{
	Use:   "schema",
	Short: "Review and promote the Space's draft vocabulary",
	Long: `Formation drafts new Concept types and predicates into the Space's draft
vocabulary (kip://local/draft@0.0.0), and Maintenance reviews them. Only the
Space's owner promotes a draft onto an installed package's symbol.`,
}

var schemaDraftsCmd = &cobra.Command{
	Use:   "drafts",
	Short: "List drafted symbols and what each was promoted to",
	Args:  cobra.NoArgs,
	RunE: func(cmd *cobra.Command, args []string) error {
		response, err := newClient().SchemaDrafts(cmd.Context())
		if err != nil {
			return err
		}
		return printRPC(cmd, response)
	},
}

var schemaPromoteCmd = &cobra.Command{
	Use:   "promote <draft> <target>",
	Short: "Promote a draft symbol onto an installed symbol (management token)",
	Long: `Promote a draft symbol onto an installed symbol of the same kind. This is a
Schema migration: elements written under the draft keep their exact reference
and match as the target from then on. A draft is promoted at most once.

<draft> is the draft's local name or its kip://local/draft@0.0.0/... reference.
<target> is the installed symbol's exact reference, or a local name exactly one
installed package defines.`,
	Args: cobra.ExactArgs(2),
	RunE: func(cmd *cobra.Command, args []string) error {
		kind, _ := cmd.Flags().GetString("kind")
		if kind != "ConceptType" && kind != "PredicateType" {
			return fmt.Errorf("--kind must be ConceptType or PredicateType")
		}
		response, err := newClient().PromoteDraftSymbol(cmd.Context(), &api.PromoteDraftInput{
			Kind: kind, From: args[0], To: args[1],
		})
		if err != nil {
			return err
		}
		return printRPC(cmd, response)
	},
}

func init() {
	schemaPromoteCmd.Flags().String("kind", "", "ConceptType or PredicateType (required)")
	schemaCmd.AddCommand(schemaDraftsCmd, schemaPromoteCmd)
	rootCmd.AddCommand(schemaCmd)
}
