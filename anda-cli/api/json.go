package api

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
)

// DecodeJSON preserves integers in dynamic payloads such as KIP parameters,
// tool results and document metadata. Custom codecs use this too: configuring
// only the outer decoder would still round numbers inside UnmarshalJSON.
func DecodeJSON(data []byte, value any) error {
	decoder := json.NewDecoder(bytes.NewReader(data))
	decoder.UseNumber()
	if err := decoder.Decode(value); err != nil {
		return err
	}
	var extra any
	if err := decoder.Decode(&extra); err != io.EOF {
		return fmt.Errorf("JSON must contain one value")
	}
	return nil
}
