// Generate deterministic interoperability fixtures using an independent Go implementation.
// Run from this directory: go run .
package main

import (
	"bytes"
	"fmt"
	"os"

	"github.com/cloudflare/circl/sign"
	"github.com/cloudflare/circl/sign/schemes"
	"github.com/cloudflare/circl/sign/slhdsa"
)

func main() {
	for _, name := range []string{"ML-DSA-44", "SLH-DSA-SHA2-128s"} {
		s := schemes.ByName(name)
		if s == nil {
			panic(name)
		}
		pk, sk := s.DeriveKey(bytes.Repeat([]byte{0x42}, s.SeedSize()))
		key, err := pk.MarshalBinary()
		if err != nil {
			panic(err)
		}
		message := bytes.Repeat([]byte{0x37}, 32)
		for _, context := range []string{"", "reth-bsc-test-context"} {
			opts := &sign.SignatureOpts{Context: context}
			var sig []byte
			if key, ok := sk.(slhdsa.PrivateKey); ok {
				sig, err = slhdsa.SignDeterministic(&key, slhdsa.NewMessage(message), []byte(context))
				if err != nil {
					panic(err)
				}
			} else {
				sig = s.Sign(sk, message, opts)
			}
			if !s.Verify(pk, message, sig, opts) {
				panic("CIRCL self verification failed")
			}
			if context != "" && s.Verify(pk, message, sig, nil) {
				panic("context not bound")
			}
			suffix := "valid"
			if context != "" {
				suffix = "context"
			}
			input := append(append(append([]byte{}, message...), sig...), key...)
			path := "../" + name + "-" + suffix + ".bin"
			if err := os.WriteFile(path, input, 0644); err != nil {
				panic(err)
			}
			fmt.Println(path, len(input))
		}
	}
}
