package main

import (
	"fmt"
	_ "github.com/joho/godotenv/autoload"
	"github.com/ldclabs/anda-brain/anda-cli/cmd"
	"os"
)

func main() {
	if err := cmd.Execute(); err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		os.Exit(1)
	}
}
