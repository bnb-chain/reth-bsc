// Run from tests/fixtures/generate: go run ./cache
// Seventeen distinct, deterministic public keys exceed the 16-entry Rust cache.
package main

import (
	"bytes"
	"fmt"
	"os"

	"github.com/cloudflare/circl/sign/schemes"
	"github.com/cloudflare/circl/sign/slhdsa"
)

func main() {
	if err := os.MkdirAll("../cache", 0755); err != nil {
		panic(err)
	}
	for _, name := range []string{"ML-DSA-44", "SLH-DSA-SHA2-128s"} {
		s := schemes.ByName(name)
		var corpus []byte
		for index := 0; index < 17; index++ {
			pk, sk := s.DeriveKey(bytes.Repeat([]byte{byte(index + 1)}, s.SeedSize()))
			key, err := pk.MarshalBinary()
			if err != nil {
				panic(err)
			}
			message := bytes.Repeat([]byte{byte(index + 128)}, 32)
			var sig []byte
			if key, ok := sk.(slhdsa.PrivateKey); ok {
				sig, err = slhdsa.SignDeterministic(&key, slhdsa.NewMessage(message), nil)
				if err != nil {
					panic(err)
				}
			} else {
				sig = s.Sign(sk, message, nil)
			}
			if !s.Verify(pk, message, sig, nil) {
				panic("invalid generated signature")
			}
			corpus = append(corpus, message...)
			corpus = append(corpus, sig...)
			corpus = append(corpus, key...)
		}
		path := "../cache/" + name + "-17-keys.bin"
		if err := os.WriteFile(path, corpus, 0644); err != nil {
			panic(err)
		}
		fmt.Println(path, len(corpus))
	}
}
