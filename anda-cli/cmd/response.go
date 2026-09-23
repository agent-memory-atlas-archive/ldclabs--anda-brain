package cmd

import (
	"fmt"

	"github.com/ldclabs/anda-brain/anda-cli/api"
	"github.com/spf13/cobra"
)

func printRPC[T any](cmd *cobra.Command, response *api.RpcResponse[T]) error {
	if response == nil {
		return fmt.Errorf("empty RPC response")
	}
	if response.Error != nil {
		return response.Error
	}
	if err := printJSON(cmd, response.Result); err != nil {
		return err
	}
	if response.NextCursor != "" {
		if _, err := fmt.Fprintf(cmd.ErrOrStderr(), "\nNext cursor: %s\n", response.NextCursor); err != nil {
			return err
		}
	}
	if response.Result != nil {
		if result, ok := any(response.Result).(interface{ Failure() error }); ok {
			return result.Failure()
		}
	}
	return nil
}
