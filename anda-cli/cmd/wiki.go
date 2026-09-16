package cmd

import (
	"fmt"
	"os"
	"strconv"
	"strings"

	"github.com/ldclabs/anda-brain/anda-cli/api"
	"github.com/spf13/cobra"
)

var wikiCmd = &cobra.Command{Use: "wiki", Short: "Manage versioned wiki documents and citations"}

func wikiDocID(raw string) uint64 {
	id, err := strconv.ParseUint(raw, 10, 64)
	if err != nil || id == 0 {
		exitError(fmt.Errorf("invalid document ID %q", raw))
	}
	return id
}

func wikiPageFlags(command *cobra.Command) {
	command.Flags().String("cursor", "", "Pagination cursor")
	command.Flags().Int("limit", 0, "Page size")
}

func wikiPage(command *cobra.Command) (string, int) {
	cursor, _ := command.Flags().GetString("cursor")
	limit, _ := command.Flags().GetInt("limit")
	if limit < 0 {
		exitError(fmt.Errorf("--limit must be non-negative"))
	}
	return cursor, limit
}

func wikiCommitCommand() *cobra.Command {
	command := &cobra.Command{
		Use: "commit", Short: "Create or update a wiki document", Args: cobra.NoArgs,
		Run: func(cmd *cobra.Command, args []string) {
			inputArg, _ := cmd.Flags().GetString("input")
			file, _ := cmd.Flags().GetString("file")
			var input api.WikiCommitInput
			if inputArg != "" {
				if file != "" {
					exitError(fmt.Errorf("--input and --file are mutually exclusive"))
				}
				var err error
				input, err = readJSONObject[api.WikiCommitInput](inputArg)
				if err != nil {
					exitError(err)
				}
			} else {
				if file == "" {
					exitError(fmt.Errorf("--input or --file is required"))
				}
				content, err := os.ReadFile(file)
				if err != nil {
					exitError(err)
				}
				input.Content = string(content)
				input.Title, _ = cmd.Flags().GetString("title")
				if input.Title == "" {
					exitError(fmt.Errorf("--title is required with --file"))
				}
				input.Namespace, _ = cmd.Flags().GetString("namespace")
				input.Slug, _ = cmd.Flags().GetString("slug")
				if cmd.Flags().Changed("tags") {
					tags, _ := cmd.Flags().GetStringSlice("tags")
					input.Tags = &tags
				}
				clearTags, _ := cmd.Flags().GetBool("clear-tags")
				if clearTags {
					if cmd.Flags().Changed("tags") {
						exitError(fmt.Errorf("--clear-tags and --tags are mutually exclusive"))
					}
					empty := []string{}
					input.Tags = &empty
				}
				if cmd.Flags().Changed("doc-id") {
					id, _ := cmd.Flags().GetUint64("doc-id")
					input.DocID = &id
				}
				if cmd.Flags().Changed("parent-version") {
					v, _ := cmd.Flags().GetUint64("parent-version")
					input.ParentVersion = &v
				}
				if cmd.Flags().Changed("acl-label") {
					v, _ := cmd.Flags().GetString("acl-label")
					input.ACLLabel = &v
				}
				if cmd.Flags().Changed("source-uri") {
					v, _ := cmd.Flags().GetString("source-uri")
					input.SourceURI = &v
				}
				if cmd.Flags().Changed("message") {
					v, _ := cmd.Flags().GetString("message")
					input.Message = &v
				}
			}
			response, err := newClient().WikiCommit(cmd.Context(), &input)
			if err != nil {
				exitError(err)
			}
			printRPC(cmd, response)
		},
	}
	command.Flags().String("input", "", "Full WikiCommitInput JSON or @file")
	command.Flags().String("file", "", "Markdown content file")
	command.Flags().String("title", "", "Document title with --file")
	command.Flags().String("namespace", "", "Document namespace")
	command.Flags().String("slug", "", "Display slug")
	command.Flags().StringSlice("tags", nil, "Document tags")
	command.Flags().Bool("clear-tags", false, "Clear stored tags on an update")
	command.Flags().Uint64("doc-id", 0, "Existing document ID for CAS update")
	command.Flags().Uint64("parent-version", 0, "Current version for CAS update")
	command.Flags().String("acl-label", "", "ACL label; empty value clears it")
	command.Flags().String("source-uri", "", "Source URI")
	command.Flags().String("message", "", "Commit message")
	return command
}

func wikiListCommand() *cobra.Command {
	command := &cobra.Command{
		Use: "list", Short: "List wiki documents", Args: cobra.NoArgs,
		Run: func(cmd *cobra.Command, args []string) {
			cursor, limit := wikiPage(cmd)
			query := api.WikiListDocsQuery{Cursor: cursor, Limit: limit}
			query.Namespace, _ = cmd.Flags().GetString("namespace")
			query.Status, _ = cmd.Flags().GetString("status")
			query.Tag, _ = cmd.Flags().GetString("tag")
			response, err := newClient().WikiListDocs(cmd.Context(), query)
			if err != nil {
				exitError(err)
			}
			printRPC(cmd, response)
		},
	}
	command.Flags().String("namespace", "", "Filter namespace")
	command.Flags().String("status", "", "Filter status: active or archived")
	command.Flags().String("tag", "", "Filter tag")
	wikiPageFlags(command)
	return command
}

func wikiReadCommand() *cobra.Command {
	command := &cobra.Command{
		Use: "read <doc_id>", Short: "Read wiki content by version, section, or byte range", Args: cobra.ExactArgs(1),
		Run: func(cmd *cobra.Command, args []string) {
			query := api.WikiReadQuery{}
			if cmd.Flags().Changed("version") {
				v, _ := cmd.Flags().GetUint64("version")
				query.Version = &v
			}
			query.Anchor, _ = cmd.Flags().GetString("anchor")
			startChanged := cmd.Flags().Changed("start")
			endChanged := cmd.Flags().Changed("end")
			if startChanged != endChanged {
				exitError(fmt.Errorf("--start and --end must be provided together"))
			}
			if query.Anchor != "" && startChanged {
				exitError(fmt.Errorf("--anchor and byte range are mutually exclusive"))
			}
			if startChanged {
				start, _ := cmd.Flags().GetUint64("start")
				end, _ := cmd.Flags().GetUint64("end")
				query.Start, query.End = &start, &end
			}
			response, err := newClient().WikiRead(cmd.Context(), wikiDocID(args[0]), query)
			if err != nil {
				exitError(err)
			}
			printRPC(cmd, response)
		},
	}
	command.Flags().Uint64("version", 0, "Historical version ID")
	command.Flags().String("anchor", "", "Section anchor")
	command.Flags().Uint64("start", 0, "Start byte offset")
	command.Flags().Uint64("end", 0, "End byte offset")
	return command
}

func wikiSearchCommand() *cobra.Command {
	command := &cobra.Command{
		Use: "search <query>", Short: "Search wiki passages", Args: cobra.ExactArgs(1),
		Run: func(cmd *cobra.Command, args []string) {
			input := &api.WikiSearchInput{Query: args[0]}
			input.Namespaces, _ = cmd.Flags().GetStringSlice("namespaces")
			input.Tags, _ = cmd.Flags().GetStringSlice("tags")
			input.Mode, _ = cmd.Flags().GetString("mode")
			docIDs, _ := cmd.Flags().GetStringSlice("doc-ids")
			for _, raw := range docIDs {
				input.DocIDs = append(input.DocIDs, wikiDocID(strings.TrimSpace(raw)))
			}
			if cmd.Flags().Changed("top-k") {
				v, _ := cmd.Flags().GetInt("top-k")
				input.TopK = &v
			}
			if cmd.Flags().Changed("expand") {
				v, _ := cmd.Flags().GetUint8("expand")
				input.Expand = &v
			}
			response, err := newClient().WikiSearch(cmd.Context(), input)
			if err != nil {
				exitError(err)
			}
			printRPC(cmd, response)
		},
	}
	command.Flags().StringSlice("namespaces", nil, "Namespaces to search")
	command.Flags().StringSlice("doc-ids", nil, "Document IDs to search")
	command.Flags().StringSlice("tags", nil, "Tags to search")
	command.Flags().Int("top-k", 0, "Maximum hits (1-50)")
	command.Flags().String("mode", "", "Search mode: chunks or docs")
	command.Flags().Uint8("expand", 0, "Neighbor expansion (0-2)")
	return command
}

func wikiVerifyCommand() *cobra.Command {
	command := &cobra.Command{
		Use: "verify", Short: "Verify a wiki citation", Args: cobra.NoArgs,
		Run: func(cmd *cobra.Command, args []string) {
			inputArg, _ := cmd.Flags().GetString("input")
			uri, _ := cmd.Flags().GetString("uri")
			var input api.WikiVerifyInput
			if inputArg != "" {
				if uri != "" {
					exitError(fmt.Errorf("--input and --uri are mutually exclusive"))
				}
				var err error
				input, err = readJSONObject[api.WikiVerifyInput](inputArg)
				if err != nil {
					exitError(err)
				}
			} else if uri != "" {
				input.URI = uri
			} else {
				exitError(fmt.Errorf("--uri or --input is required"))
			}
			response, err := newClient().WikiVerify(cmd.Context(), &input)
			if err != nil {
				exitError(err)
			}
			printRPC(cmd, response)
		},
	}
	command.Flags().String("uri", "", "wiki:// citation URI")
	command.Flags().String("input", "", "Full WikiVerifyInput JSON or @file")
	return command
}

func wikiEventsCommand() *cobra.Command {
	command := &cobra.Command{
		Use: "events", Short: "List wiki audit events", Args: cobra.NoArgs,
		Run: func(cmd *cobra.Command, args []string) {
			cursor, limit := wikiPage(cmd)
			query := api.WikiEventsQuery{Cursor: cursor, Limit: limit}
			query.Kind, _ = cmd.Flags().GetString("kind")
			if cmd.Flags().Changed("doc-id") {
				id, _ := cmd.Flags().GetUint64("doc-id")
				query.DocID = &id
			}
			response, err := newClient().WikiEvents(cmd.Context(), query)
			if err != nil {
				exitError(err)
			}
			printRPC(cmd, response)
		},
	}
	command.Flags().String("kind", "", "Event kind")
	command.Flags().Uint64("doc-id", 0, "Document ID")
	wikiPageFlags(command)
	return command
}

func wikiImportCommand() *cobra.Command {
	command := &cobra.Command{
		Use: "import", Short: "Import an OKF bundle (requires full scope)", Args: cobra.NoArgs,
		Run: func(cmd *cobra.Command, args []string) {
			inputArg, _ := cmd.Flags().GetString("input")
			if inputArg == "" {
				exitError(fmt.Errorf("--input is required"))
			}
			input, err := readJSONObject[api.WikiImportInput](inputArg)
			if err != nil {
				exitError(err)
			}
			response, err := newClient().WikiImport(cmd.Context(), &input)
			if err != nil {
				exitError(err)
			}
			printRPC(cmd, response)
		},
	}
	command.Flags().String("input", "", "WikiImportInput JSON or @file")
	return command
}

func init() {
	wikiCmd.AddCommand(wikiCommitCommand(), wikiListCommand(), wikiReadCommand(), wikiSearchCommand(), wikiVerifyCommand(), wikiEventsCommand(), wikiImportCommand())
	wikiCmd.AddCommand(&cobra.Command{
		Use: "get <doc_id>", Short: "Get document metadata and table of contents", Args: cobra.ExactArgs(1),
		Run: func(cmd *cobra.Command, args []string) {
			response, err := newClient().WikiGetDoc(cmd.Context(), wikiDocID(args[0]))
			if err != nil {
				exitError(err)
			}
			printRPC(cmd, response)
		},
	})
	versions := &cobra.Command{
		Use: "versions <doc_id>", Short: "List document versions", Args: cobra.ExactArgs(1),
		Run: func(cmd *cobra.Command, args []string) {
			cursor, limit := wikiPage(cmd)
			response, err := newClient().WikiVersions(cmd.Context(), wikiDocID(args[0]), cursor, limit)
			if err != nil {
				exitError(err)
			}
			printRPC(cmd, response)
		},
	}
	wikiPageFlags(versions)
	wikiCmd.AddCommand(versions)
	for _, action := range []string{"archive", "restore"} {
		action := action
		wikiCmd.AddCommand(&cobra.Command{
			Use: action + " <doc_id>", Short: action + " a wiki document", Args: cobra.ExactArgs(1),
			Run: func(cmd *cobra.Command, args []string) {
				client := newClient()
				var response *api.RpcResponse[api.WikiDocInfo]
				var err error
				if action == "archive" {
					response, err = client.WikiArchive(cmd.Context(), wikiDocID(args[0]))
				} else {
					response, err = client.WikiRestore(cmd.Context(), wikiDocID(args[0]))
				}
				if err != nil {
					exitError(err)
				}
				printRPC(cmd, response)
			},
		})
	}
	export := &cobra.Command{
		Use: "export", Short: "Export an OKF namespace (requires full scope)", Args: cobra.NoArgs,
		Run: func(cmd *cobra.Command, args []string) {
			namespace, _ := cmd.Flags().GetString("namespace")
			response, err := newClient().WikiExport(cmd.Context(), namespace)
			if err != nil {
				exitError(err)
			}
			printRPC(cmd, response)
		},
	}
	export.Flags().String("namespace", "", "Namespace to export (default: default)")
	wikiCmd.AddCommand(export)
	wikiCmd.AddCommand(&cobra.Command{
		Use: "digest", Short: "Digest pending wiki versions into memory", Args: cobra.NoArgs,
		Run: func(cmd *cobra.Command, args []string) {
			response, err := newClient().WikiDigest(cmd.Context())
			if err != nil {
				exitError(err)
			}
			printRPC(cmd, response)
		},
	})
	rootCmd.AddCommand(wikiCmd)
}
