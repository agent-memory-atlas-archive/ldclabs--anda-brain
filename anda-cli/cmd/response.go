package cmd

import (
	"fmt"

	"github.com/ldclabs/anda-brain/anda-cli/api"
	"github.com/spf13/cobra"
)

func printRPC[T any](cmd *cobra.Command, response *api.RpcResponse[T]) {
	if response.Error != nil {
		exitError(response.Error)
	}
	printJSON(response.Result)
	if response.NextCursor != "" {
		fmt.Fprintf(cmd.ErrOrStderr(), "\nNext cursor: %s\n", response.NextCursor)
	}
}
