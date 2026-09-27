package cmd

import (
	"path/filepath"
	"strings"
	"testing"

	"github.com/spf13/cobra"
)

func TestReadInputFromFlagFileOrStdin(t *testing.T) {
	newCommand := func(stdin string, flags ...string) *cobra.Command {
		command := &cobra.Command{}
		command.Flags().String("text", "", "")
		command.Flags().String("file", "", "")
		command.SetIn(strings.NewReader(stdin))
		for i := 0; i+1 < len(flags); i += 2 {
			if err := command.Flags().Set(flags[i], flags[i+1]); err != nil {
				t.Fatal(err)
			}
		}
		return command
	}
	path := filepath.Join(t.TempDir(), "input.txt")
	mustWriteFile(t, path, "from file")
	for _, tc := range []struct {
		command *cobra.Command
		want    string
	}{
		{newCommand("from stdin"), "from stdin"},
		{newCommand("from stdin", "text", "inline"), "inline"},
		{newCommand("from stdin", "file", path), "from file"},
	} {
		if got, err := readInput(tc.command, "text", "file"); err != nil || got != tc.want {
			t.Fatalf("got %q (%v), want %q", got, err, tc.want)
		}
	}
	if _, err := readInput(newCommand("", "text", "inline", "file", path), "text", "file"); err == nil ||
		!strings.Contains(err.Error(), "cannot be used together") {
		t.Fatalf("both inputs accepted: %v", err)
	}
}
