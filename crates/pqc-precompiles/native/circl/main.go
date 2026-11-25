// Package main exposes only public-input verification through a C ABI.
package main

/*
#include <stdint.h>
#include <stddef.h>
*/
import "C"

import (
	"runtime/cgo"
	"unsafe"

	"github.com/cloudflare/circl/sign/mldsa/mldsa44"
	"github.com/cloudflare/circl/sign/slhdsa"
)

// bsc_pqc_circl_verify returns 1 for valid, 0 for invalid, and -1 for a backend
// failure. The caller must supply readable buffers of the declared lengths.
// Buffers are borrowed synchronously, never written to or retained. No Go
// pointer crosses the ABI. All key/verification state is local to this call.
//
//export bsc_pqc_circl_verify
func bsc_pqc_circl_verify(algorithm C.uint32_t, message *C.uint8_t, messageLen C.size_t, signature *C.uint8_t, signatureLen C.size_t, key *C.uint8_t, keyLen C.size_t) (result C.int) {
	// Recoverable Go panics must not escape into Rust or become false results.
	defer func() {
		if recover() != nil {
			result = -1
		}
	}()
	var sigSize, keySize C.size_t
	switch algorithm {
	case 0:
		sigSize, keySize = 2420, 1312
	case 1:
		sigSize, keySize = 7856, 32
	default:
		return -1
	}
	// Bound lengths before converting size_t to int or creating slices.
	if messageLen != 32 || signatureLen != sigSize || keyLen != keySize {
		return 0
	}
	if message == nil || signature == nil || key == nil {
		return -1
	}
	msg := unsafe.Slice((*byte)(unsafe.Pointer(message)), int(messageLen))
	sig := unsafe.Slice((*byte)(unsafe.Pointer(signature)), int(signatureLen))
	pk := unsafe.Slice((*byte)(unsafe.Pointer(key)), int(keyLen))
	var valid bool
	if algorithm == 0 {
		var publicKey mldsa44.PublicKey
		if publicKey.UnmarshalBinary(pk) != nil {
			return 0
		}
		valid = mldsa44.Verify(&publicKey, msg, nil, sig)
	} else {
		publicKey := slhdsa.PublicKey{ID: slhdsa.SHA2_128s}
		if publicKey.UnmarshalBinary(pk) != nil {
			return 0
		}
		valid = slhdsa.Verify(&publicKey, slhdsa.NewMessage(msg), sig, nil)
	}
	if valid {
		return 1
	}
	return 0
}

func main() {}

// Decoded keys own their Go memory. Verification only reads them; temporary
// signature and hash state is allocated by CIRCL for each call.
type preparedKey struct {
	algorithm uint32
	ml        *mldsa44.PublicKey
	slh       *slhdsa.PublicKey
}

// bsc_pqc_circl_key_new copies/decodes the public key into Go-owned memory and
// returns an opaque integer handle. Rust must delete it exactly once after the
// last verification. A zero handle means a backend/argument failure.
//
//export bsc_pqc_circl_key_new
func bsc_pqc_circl_key_new(algorithm C.uint32_t, key *C.uint8_t, keyLen C.size_t) (result C.uintptr_t) {
	defer func() {
		if recover() != nil {
			result = 0
		}
	}()
	if key == nil || (algorithm != 0 && algorithm != 1) {
		return 0
	}
	if (algorithm == 0 && keyLen != 1312) || (algorithm == 1 && keyLen != 32) {
		return 0
	}
	pk := unsafe.Slice((*byte)(unsafe.Pointer(key)), int(keyLen))
	prepared := &preparedKey{algorithm: uint32(algorithm)}
	if algorithm == 0 {
		prepared.ml = new(mldsa44.PublicKey)
		if prepared.ml.UnmarshalBinary(pk) != nil {
			return 0
		}
	} else {
		prepared.slh = &slhdsa.PublicKey{ID: slhdsa.SHA2_128s}
		if prepared.slh.UnmarshalBinary(pk) != nil {
			return 0
		}
	}
	return C.uintptr_t(cgo.NewHandle(prepared))
}

//export bsc_pqc_circl_key_free
func bsc_pqc_circl_key_free(handle C.uintptr_t) (result C.int) {
	defer func() {
		if recover() != nil {
			result = -1
		}
	}()
	cgo.Handle(handle).Delete()
	return 1
}

//export bsc_pqc_circl_verify_prepared
func bsc_pqc_circl_verify_prepared(handle C.uintptr_t, message *C.uint8_t, messageLen C.size_t, signature *C.uint8_t, signatureLen C.size_t) (result C.int) {
	defer func() {
		if recover() != nil {
			result = -1
		}
	}()
	prepared := cgo.Handle(handle).Value().(*preparedKey)
	sigSize := C.size_t(2420)
	if prepared.algorithm == 1 {
		sigSize = 7856
	}
	if messageLen != 32 || signatureLen != sigSize {
		return 0
	}
	if message == nil || signature == nil {
		return -1
	}
	msg := unsafe.Slice((*byte)(unsafe.Pointer(message)), int(messageLen))
	sig := unsafe.Slice((*byte)(unsafe.Pointer(signature)), int(signatureLen))
	var valid bool
	if prepared.algorithm == 0 {
		valid = mldsa44.Verify(prepared.ml, msg, nil, sig)
	} else {
		valid = slhdsa.Verify(prepared.slh, slhdsa.NewMessage(msg), sig, nil)
	}
	if valid {
		return 1
	}
	return 0
}
