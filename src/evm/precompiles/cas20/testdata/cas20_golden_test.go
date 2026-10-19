package vm

import (
	"encoding/hex"
	"encoding/json"
	"errors"
	"os"
	"testing"

	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/crypto"
	"github.com/holiman/uint256"
)

// TestRecordCAS20Golden records a scripted CAS20 scenario so another client can
// replay it call by call: every call's returndata, status, gas, refund delta and
// logs, and the state root the harness ends on. It is the conformance fixture
// reth-bsc's `cas20::tests::golden` replays. Skipped unless CAS20_GOLDEN names the
// output file:
//
//	CAS20_GOLDEN=/tmp/cas20_golden.json go test -run TestRecordCAS20Golden ./core/vm
//
// The harness runs on params.TestChainConfig (chain id 1) with the access list and
// committed state of a single transaction that never finalises: nothing is warm
// and every original value is zero at the start. A replay must reproduce that.
func TestRecordCAS20Golden(t *testing.T) {
	out := os.Getenv("CAS20_GOLDEN")
	if out == "" {
		t.Skip("set CAS20_GOLDEN=<file> to record")
	}
	statedb, evm := newCAS20EVM(t)
	chainID := evm.chainConfig.ChainID.Uint64()
	base := uint64(1_800_000_000)
	evm.Context.Time = base
	var steps []goldenStep
	alice := common.HexToAddress("0xa11ce0")
	bob := common.HexToAddress("0xb0b0")
	carol := common.HexToAddress("0xca201")
	gov := common.HexToAddress("0x1007")
	admin := cas20TestCaller
	hx := func(b []byte) string { return "0x" + hex.EncodeToString(b) }

	run := func(name, kind string, caller, to common.Address, input []byte, gas uint64, value *uint256.Int) []byte {
		if value == nil {
			value = new(uint256.Int)
		}
		logsBefore := len(statedb.Logs())
		refundBefore := statedb.GetRefund()
		budget := NewGasBudget(gas)
		var ret []byte
		var left GasBudget
		var err error
		switch kind {
		case "call":
			ret, left, err = evm.Call(caller, to, input, budget, value)
		case "static":
			ret, left, err = evm.StaticCall(caller, to, input, budget)
		case "delegate":
			ret, left, err = evm.DelegateCall(caller, caller, to, input, budget, value)
		}
		status := "ok"
		switch {
		case err == nil:
		case errors.Is(err, ErrExecutionReverted):
			status = "revert"
		case errors.Is(err, ErrOutOfGas):
			status = "oog"
		default:
			status = "err:" + err.Error()
		}
		var logs []goldenLog
		for _, l := range statedb.Logs()[logsBefore:] {
			gl := goldenLog{Address: l.Address.Hex(), Data: hx(l.Data)}
			for _, tp := range l.Topics {
				gl.Topics = append(gl.Topics, tp.Hex())
			}
			logs = append(logs, gl)
		}
		steps = append(steps, goldenStep{
			Name: name, Kind: kind, Caller: caller.Hex(), To: to.Hex(), Input: hx(input), Gas: gas,
			Value: value.Hex(), Time: evm.Context.Time, Ret: hx(ret), Status: status,
			GasUsed: gas - left.RegularGas, Refund: int64(statedb.GetRefund()) - int64(refundBefore), Logs: logs,
		})
		return ret
	}
	call := func(name string, caller, to common.Address, input []byte) []byte {
		return run(name, "call", caller, to, input, 5_000_000, nil)
	}
	static := func(name string, caller, to common.Address, input []byte) []byte {
		return run(name, "static", caller, to, input, 5_000_000, nil)
	}
	str := func(sel [4]byte, parts ...abiPart) []byte {
		return append(append([]byte{}, sel[:]...), encodeTuple(parts...)...)
	}
	bytesArray := func(calls [][]byte) abiPart {
		elems := make([][]byte, len(calls))
		for i, c := range calls {
			elems[i] = append(u256hash(uint64(len(c))).Bytes(), rightPad32(c)...)
		}
		arr := append([]byte{}, u256hash(uint64(len(calls))).Bytes()...)
		cur := uint64(len(calls) * 32)
		for _, e := range elems {
			arr = append(arr, u256hash(cur).Bytes()...)
			cur += uint64(len(e))
		}
		for _, e := range elems {
			arr = append(arr, e...)
		}
		return abiPart{dynamic: true, tail: arr}
	}

	static("factory.isCAS20(factory)", bob, CAS20FactoryAddress, cas20Call(selIsCAS20, addrKey(CAS20FactoryAddress)))
	static("factory.getCAS20Address", bob, CAS20FactoryAddress, cas20Call(selGetCAS20Address, u256hash(0), addrKey(alice), u256hash(1)))
	ret := call("create.asset", alice, CAS20FactoryAddress, encodeCreateCAS20(cas20VariantAsset, u256hash(1), alice, [][]byte{
		cas20Call(selGrantRole, roleMint, addrKey(alice)),
		cas20Call(selGrantRole, roleOperator, addrKey(alice)),
		cas20Call(selGrantRole, rolePause, addrKey(alice)),
		cas20Call(selGrantRole, roleUnpause, addrKey(alice)),
		cas20Call(selGrantRole, roleSeize, addrKey(alice)),
		cas20Call(selGrantRole, roleMetadata, addrKey(alice)),
		cas20Call(selGrantRole, roleBurn, addrKey(alice)),
		cas20Call(selMint, addrKey(alice), u256hash(1000)),
	}))
	token := common.BytesToAddress(ret)
	static("name", bob, token, cas20Call(selName))
	static("symbol", bob, token, cas20Call(selSymbol))
	static("decimals", bob, token, cas20Call(selDecimals))
	static("totalSupply", bob, token, cas20Call(selTotalSupply))
	static("balanceOf(alice)", bob, token, cas20Call(selBalanceOf, addrKey(alice)))
	call("transfer alice->bob 400", alice, token, cas20Call(selTransfer, addrKey(bob), u256hash(400)))
	call("transfer bob->alice 401 (insufficient)", bob, token, cas20Call(selTransfer, addrKey(alice), u256hash(401)))
	call("approve alice->bob 30", alice, token, cas20Call(selApprove, addrKey(bob), u256hash(30)))
	call("transferFrom bob 10", bob, token, cas20Call(selTransferFrom, addrKey(alice), addrKey(bob), u256hash(10)))
	call("transferFrom bob 31 (allowance)", bob, token, cas20Call(selTransferFrom, addrKey(alice), addrKey(bob), u256hash(31)))
	static("allowance", bob, token, cas20Call(selAllowance, addrKey(alice), addrKey(bob)))
	call("approve alice->bob 0 (clear)", alice, token, cas20Call(selApprove, addrKey(bob), u256hash(0)))
	call("mint alice->bob 5", alice, token, cas20Call(selMint, addrKey(bob), u256hash(5)))
	call("mint by bob (no role)", bob, token, cas20Call(selMint, addrKey(bob), u256hash(5)))
	call("burn alice 100", alice, token, cas20Call(selBurn, u256hash(100)))
	call("burnWithMemo alice 1", alice, token, cas20Call(selBurnWithMemo, u256hash(1), common.HexToHash("0x11")))
	call("mintWithMemo alice->carol 7", alice, token, cas20Call(selMintWithMemo, addrKey(carol), u256hash(7), common.HexToHash("0x22")))
	call("pause [transfer]", alice, token, cas20CallU8Array(selPause, byte(cas20PauseTransfer)))
	call("transfer while paused", alice, token, cas20Call(selTransfer, addrKey(bob), u256hash(1)))
	static("pausedFeatures", bob, token, cas20Call(selPausedFeatures))
	static("isPaused(0)", bob, token, cas20Call(selIsPaused, u256hash(0)))
	call("unpause [transfer]", alice, token, cas20CallU8Array(selUnpause, byte(cas20PauseTransfer)))
	call("pause by bob (no role)", bob, token, cas20CallU8Array(selPause, byte(cas20PauseMint)))
	longName := "Renamed Token With A Long Name Exceeding Thirty One Bytes For Sure"
	call("updateName long", alice, token, str(selUpdateName, abiString(longName)))
	static("name after rename", bob, token, cas20Call(selName))
	call("updateSymbol", alice, token, str(selUpdateSymbol, abiString("RTK")))
	call("updateContractURI", alice, token, str(selUpdateContractURI, abiString("ipfs://bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi/meta.json")))
	static("contractURI", bob, token, cas20Call(selContractURI))
	call("updateName short", alice, token, str(selUpdateName, abiString("Short")))
	static("eip712Domain", bob, token, cas20Call(selEIP712Domain))
	static("DOMAIN_SEPARATOR", bob, token, cas20Call(selDomainSeparator))
	call("updateExtraMetadata", alice, token, str(selUpdateExtraMetadata, abiString("k"), abiString("a value that is longer than thirty two bytes to hit the chunked path")))
	static("extraMetadata(k)", bob, token, str(selExtraMetadata, abiString("k")))
	call("updateExtraMetadata empty key", alice, token, str(selUpdateExtraMetadata, abiString(""), abiString("v")))
	static("multiplier", bob, token, cas20Call(selMultiplier))
	call("updateUIMultiplier 2x at +3600", alice, token, cas20Call(selUpdateUIMultiplier, u256hash(2e18), u256hash(base+3600)))
	call("updateUIMultiplier again (exists)", alice, token, cas20Call(selUpdateUIMultiplier, u256hash(3e18), u256hash(base+7200)))
	static("newUIMultiplier", bob, token, cas20Call(selNewUIMultiplier))
	static("effectiveAt", bob, token, cas20Call(selEffectiveAt))
	static("balanceOfUI(bob)", bob, token, cas20Call(selBalanceOfUI, addrKey(bob)))
	evm.Context.Time = base + 3600
	static("uiMultiplier matured", bob, token, cas20Call(selUIMultiplier))
	static("totalSupplyUI matured", bob, token, cas20Call(selTotalSupplyUI))
	static("newUIMultiplier matured", bob, token, cas20Call(selNewUIMultiplier))
	call("cancelUIMultiplierUpdate (matured)", alice, token, cas20Call(selCancelUIMultiplier))
	call("updateMultiplier 1.5x", alice, token, cas20Call(selUpdateMultiplier, u256hash(1.5e18)))
	evm.Context.Time = base + 3601
	call("updateUIMultiplier 4x at +9000", alice, token, cas20Call(selUpdateUIMultiplier, u256hash(4e18), u256hash(base+9000)))
	call("cancelUIMultiplierUpdate (live)", alice, token, cas20Call(selCancelUIMultiplier))
	static("supportsInterface(IERC165)", bob, token, cas20Call(selSupportsInterface, common.HexToHash("0x01ffc9a700000000000000000000000000000000000000000000000000000000")))
	static("supportsInterface(dirty)", bob, token, cas20Call(selSupportsInterface, common.HexToHash("0x01ffc9a700000000000000000000000000000000000000000000000000000001")))
	static("toUIAmount(100)", bob, token, cas20Call(selToUIAmount, u256hash(100)))
	static("fromUIAmount(150)", bob, token, cas20Call(selFromUIAmount, u256hash(150)))
	call("batchMint", alice, token, str(selBatchMint, abiWordArray([]common.Hash{addrKey(bob), addrKey(carol)}), abiWordArray([]common.Hash{u256hash(1), u256hash(2)})))
	call("batchMint mismatch", alice, token, str(selBatchMint, abiWordArray([]common.Hash{addrKey(bob)}), abiWordArray([]common.Hash{u256hash(1), u256hash(2)})))
	call("announce", alice, token, str(selAnnounce, bytesArray([][]byte{cas20Call(selDefaultAdminRole), cas20Call(selMint, addrKey(bob), u256hash(1))}), abiString("A1"), abiString("first announcement"), abiString("https://example.com/a1")))
	static("isAnnouncementIdUsed(A1)", bob, token, str(selIsAnnouncementIdUsed, abiString("A1")))
	call("announce reused id", alice, token, str(selAnnounce, bytesArray(nil), abiString("A1"), abiString(""), abiString("")))
	call("announce with failing call", alice, token, str(selAnnounce, bytesArray([][]byte{cas20Call(selMint, addrKey(common.Address{}), u256hash(1))}), abiString("A2"), abiString(""), abiString("")))
	key, _ := crypto.ToECDSA(common.LeftPadBytes([]byte{7}, 32))
	owner := crypto.PubkeyToAddress(key.PublicKey)
	dom := cas20DomainSeparator("Short", uint256.NewInt(chainID), token)
	deadline := u256hash(base + 99_999)
	structHash := append([]byte{}, cas20PermitTypehash.Bytes()...)
	for _, wd := range []common.Hash{addrKey(owner), addrKey(bob), u256hash(55), u256hash(0), deadline} {
		structHash = append(structHash, wd.Bytes()...)
	}
	digest := crypto.Keccak256([]byte{0x19, 0x01}, dom.Bytes(), crypto.Keccak256(structHash))
	sig, _ := crypto.Sign(digest, key)
	var r, s common.Hash
	copy(r[:], sig[:32])
	copy(s[:], sig[32:64])
	v := u256hash(uint64(sig[64]) + 27)
	permit := cas20Call(selPermit, addrKey(owner), addrKey(bob), u256hash(55), deadline, v, r, s)
	call("permit", bob, token, permit)
	static("nonces(owner)", bob, token, cas20Call(selNonces, addrKey(owner)))
	static("allowance(owner,bob)", bob, token, cas20Call(selAllowance, addrKey(owner), addrKey(bob)))
	call("permit replay", bob, token, permit)
	call("transferWithMemoFormat", alice, token, cas20Call(selTransferWithMemoFormat, addrKey(bob), u256hash(1), common.HexToHash("0xaa"), common.HexToHash("0xbb")))
	call("transferWithMemoFormat zero format", alice, token, cas20Call(selTransferWithMemoFormat, addrKey(bob), u256hash(1), common.HexToHash("0xaa"), common.Hash{}))
	call("approve alice->bob 3", alice, token, cas20Call(selApprove, addrKey(bob), u256hash(3)))
	call("transferFromWithMemoFormat", bob, token, cas20Call(selTransferFromWithMemoFormat, addrKey(alice), addrKey(carol), u256hash(2), common.HexToHash("0xcc"), common.HexToHash("0xdd")))
	call("transferWithMemo", alice, token, cas20Call(selTransferWithMemo, addrKey(bob), u256hash(1), common.HexToHash("0xee")))
	reg := CAS20PolicyRegistryAddress
	ret = call("createPolicy blocklist", admin, reg, cas20Call(selCreatePolicy, addrKey(admin), u256hash(cas20PolicyBlocklist)))
	id1 := new(uint256.Int).SetBytes(ret).Uint64()
	call("updateBlocklist add bob", admin, reg, str(selUpdateBlocklist, abiWord(wU64(id1)), abiWord(wU8(1)), abiWordArray([]common.Hash{addrKey(bob)})))
	static("isAuthorized(id1,bob)", bob, reg, cas20Call(selIsAuthorized, u256hash(id1), addrKey(bob)))
	static("isAuthorized(id1,alice)", bob, reg, cas20Call(selIsAuthorized, u256hash(id1), addrKey(alice)))
	ret = call("createPolicyWithAccounts allowlist", admin, reg, str(selCreatePolicyWithAccounts, abiWord(addrKey(admin)), abiWord(wU8(cas20PolicyAllowlist)), abiWordArray([]common.Hash{addrKey(alice)})))
	id2 := new(uint256.Int).SetBytes(ret).Uint64()
	ret = call("createCompositePolicy union", admin, reg, str(selCreateComposite, abiWord(addrKey(admin)), abiWord(wU8(cas20PolicyUnion)), abiWordArray([]common.Hash{wU64(id1), wU64(id2)})))
	id3 := new(uint256.Int).SetBytes(ret).Uint64()
	static("compositePolicyChildIds", bob, reg, cas20Call(selCompositeChildIds, u256hash(id3)))
	static("isAuthorized(id3,alice)", bob, reg, cas20Call(selIsAuthorized, u256hash(id3), addrKey(alice)))
	call("updateComposite shrink", admin, reg, str(selUpdateComposite, abiWord(wU64(id3)), abiWordArray([]common.Hash{wU64(id2), wU64(id1)})))
	call("createComposite with composite child", admin, reg, str(selCreateComposite, abiWord(addrKey(admin)), abiWord(wU8(cas20PolicyIntersect)), abiWordArray([]common.Hash{wU64(id1), wU64(id3)})))
	call("stageUpdateAdmin id1->carol", admin, reg, cas20Call(selStageUpdateAdmin, u256hash(id1), addrKey(carol)))
	static("pendingPolicyAdmin(id1)", bob, reg, cas20Call(selPendingPolicyAdmin, u256hash(id1)))
	call("finalizeUpdateAdmin by bob", bob, reg, cas20Call(selFinalizeUpdateAdmin, u256hash(id1)))
	call("finalizeUpdateAdmin by carol", carol, reg, cas20Call(selFinalizeUpdateAdmin, u256hash(id1)))
	static("policyAdmin(id1)", bob, reg, cas20Call(selPolicyAdmin, u256hash(id1)))
	call("updateBlocklist by old admin", admin, reg, str(selUpdateBlocklist, abiWord(wU64(id1)), abiWord(wU8(0)), abiWordArray([]common.Hash{addrKey(bob)})))
	call("updateAllowlist wrong type", carol, reg, str(selUpdateAllowlist, abiWord(wU64(id1)), abiWord(wU8(1)), abiWordArray([]common.Hash{addrKey(bob)})))
	static("policyExists(99)", bob, reg, cas20Call(selPolicyExists, u256hash(99)))
	static("policyExists(sentinel block)", bob, reg, cas20Call(selPolicyExists, u256hash(cas20PolicyAlwaysBlock)))
	call("updatePolicy TRANSFER_SENDER=id1", alice, token, cas20Call(selUpdatePolicy, scopeTransferSender, u256hash(id1)))
	call("transfer bob->alice (forbidden)", bob, token, cas20Call(selTransfer, addrKey(alice), u256hash(1)))
	static("policyId(TRANSFER_SENDER)", bob, token, cas20Call(selPolicyId, scopeTransferSender))
	call("updatePolicy unknown id", alice, token, cas20Call(selUpdatePolicy, scopeTransferSender, u256hash(77)))
	call("updatePolicy SEIZE_EXEMPT=id1", alice, token, cas20Call(selUpdatePolicy, scopeSeizeExempt, u256hash(id1)))
	call("seizeWithMemo bob->alice 3", alice, token, cas20Call(selSeizeWithMemo, addrKey(bob), addrKey(alice), u256hash(3), common.HexToHash("0x99")))
	call("seizeWithMemo alice (not seizable)", alice, token, cas20Call(selSeizeWithMemo, addrKey(alice), addrKey(bob), u256hash(1), common.Hash{}))
	call("setRoleAdmin MINT->PAUSE", alice, token, cas20Call(selSetRoleAdmin, roleMint, rolePause))
	static("getRoleAdmin(MINT)", bob, token, cas20Call(selGetRoleAdmin, roleMint))
	call("grantRole by bob", bob, token, cas20Call(selGrantRole, roleMint, addrKey(bob)))
	call("revokeRole MINT alice", alice, token, cas20Call(selRevokeRole, roleMint, addrKey(alice)))
	call("renounceRole SEIZE", alice, token, cas20Call(selRenounceRole, roleSeize, addrKey(alice)))
	call("renounceRole bad confirmation", alice, token, cas20Call(selRenounceRole, roleBurn, addrKey(bob)))
	static("hasRole(MINT,alice)", bob, token, cas20Call(selHasRole, roleMint, addrKey(alice)))
	call("revokeRole DEFAULT_ADMIN (last)", alice, token, cas20Call(selRevokeRole, roleDefaultAdmin, addrKey(alice)))
	call("updateSupplyCap 500 (below supply)", alice, token, cas20Call(selUpdateSupplyCap, u256hash(500)))
	call("updateSupplyCap 10^6", alice, token, cas20Call(selUpdateSupplyCap, u256hash(1_000_000)))
	static("supplyCap", bob, token, cas20Call(selSupplyCap))
	call("grantRole MINT bob via PAUSE admin", alice, token, cas20Call(selGrantRole, roleMint, addrKey(bob)))
	call("mint over cap", bob, token, cas20Call(selMint, addrKey(bob), u256hash(2_000_000)))
	ret = call("create.stablecoin", bob, CAS20FactoryAddress, encodeCreateCAS20(cas20VariantStablecoin, u256hash(2), bob, [][]byte{
		cas20Call(selGrantRole, roleMint, addrKey(bob)),
	}))
	stable := common.BytesToAddress(ret)
	static("stable.currency", alice, stable, cas20Call(selCurrency))
	static("stable.decimals", alice, stable, cas20Call(selDecimals))
	static("stable.multiplier (absent)", alice, stable, cas20Call(selMultiplier))
	call("stable.mint", bob, stable, cas20Call(selMint, addrKey(alice), u256hash(50)))
	call("stable.transfer", alice, stable, cas20Call(selTransfer, addrKey(bob), u256hash(20)))
	call("create.stablecoin bad currency", bob, CAS20FactoryAddress, encodeCreateCAS20WithParams(cas20VariantStablecoin, u256hash(3), cas20StablecoinParams("S", "S", bob, "usd"), nil))
	call("create.asset bad decimals", bob, CAS20FactoryAddress, encodeCreateCAS20WithParams(cas20VariantAsset, u256hash(4), cas20AssetParams("A", "A", bob, 3), nil))
	call("create.asset again same salt", alice, CAS20FactoryAddress, encodeCreateCAS20(cas20VariantAsset, u256hash(1), alice, nil))
	act := CAS20ActivationRegistryAddress
	static("isActivated(asset)", bob, act, cas20Call(selIsActivated, featureCAS20Asset))
	call("deactivate asset", admin, act, cas20Call(selDeactivate, featureCAS20Asset))
	call("create.asset while closed", alice, CAS20FactoryAddress, encodeCreateCAS20(cas20VariantAsset, u256hash(5), alice, nil))
	call("activate asset", admin, act, cas20Call(selActivate, featureCAS20Asset))
	call("activate asset again", admin, act, cas20Call(selActivate, featureCAS20Asset))
	call("updateParam admin by gov", gov, act, str(selUpdateParam, abiString("admin"), abiBytes(carol.Bytes())))
	static("admin()", bob, act, cas20Call(selActivationAdm))
	call("updateParam by non-gov", admin, act, str(selUpdateParam, abiString("admin"), abiBytes(carol.Bytes())))
	call("updateParam unknown key", gov, act, str(selUpdateParam, abiString("other"), abiBytes(carol.Bytes())))
	static("checkActivated(stablecoin)", bob, act, cas20Call(selCheckActivated, featureCAS20Stablecoin))
	call("deactivate by old admin", admin, act, cas20Call(selDeactivate, featureCAS20Stablecoin))
	call("deactivate policy_registry by carol", carol, act, cas20Call(selDeactivate, featurePolicyRegistry))
	call("createPolicy while closed", admin, reg, cas20Call(selCreatePolicy, addrKey(admin), u256hash(cas20PolicyBlocklist)))
	run("static transfer", "static", alice, token, cas20Call(selTransfer, addrKey(bob), u256hash(1)), 5_000_000, nil)
	run("delegatecall transfer", "delegate", alice, token, cas20Call(selTransfer, addrKey(bob), u256hash(1)), 5_000_000, nil)
	run("call with value", "call", alice, token, cas20Call(selBalanceOf, addrKey(bob)), 5_000_000, uint256.NewInt(1))
	run("oog name", "call", alice, token, cas20Call(selName), 200, nil)
	run("oog transfer at sentry", "call", alice, token, cas20Call(selTransfer, addrKey(bob), u256hash(1)), 2_400, nil)
	call("short calldata", alice, token, []byte{1, 2, 3})
	call("unknown selector", alice, token, []byte{0xde, 0xad, 0xbe, 0xef})
	call("missing argument", alice, token, cas20Call(selTransfer, addrKey(bob)))
	call("uninitialized token", alice, cas20DeriveAddress(cas20VariantAsset, bob, u256hash(77)), cas20Call(selName))
	static("factory.isCAS20Initialized(ghost)", alice, CAS20FactoryAddress, cas20Call(selIsCAS20Initialized, addrKey(cas20DeriveAddress(cas20VariantAsset, bob, u256hash(77)))))
	static("factory.variantOf(token)", alice, CAS20FactoryAddress, cas20Call(selVariantOf, addrKey(token)))
	static("factory.variantOf(alice)", alice, CAS20FactoryAddress, cas20Call(selVariantOf, addrKey(alice)))
	call("renounceLastAdmin by bob (not admin)", bob, token, cas20Call(selRenounceLastAdmin))
	call("renounceLastAdmin", alice, token, cas20Call(selRenounceLastAdmin))
	call("grantRole after renounce", alice, token, cas20Call(selGrantRole, roleBurn, addrKey(bob)))

	root := statedb.IntermediateRoot(true)
	accounts := map[string]string{}
	for _, a := range []common.Address{CAS20ActivationRegistryAddress, CAS20PolicyRegistryAddress, token, stable} {
		accounts[a.Hex()] = statedb.GetCodeHash(a).Hex()
	}
	b, err := json.MarshalIndent(map[string]any{
		"chainId": chainID, "admin": admin.Hex(), "steps": steps, "stateRoot": root.Hex(), "codeHashes": accounts,
	}, "", " ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, b, 0o644); err != nil {
		t.Fatal(err)
	}
	t.Logf("%d steps, root %s", len(steps), root.Hex())
}

type goldenLog struct {
	Address string   `json:"address"`
	Topics  []string `json:"topics"`
	Data    string   `json:"data"`
}

type goldenStep struct {
	Name    string      `json:"name"`
	Kind    string      `json:"kind"`
	Caller  string      `json:"caller"`
	To      string      `json:"to"`
	Input   string      `json:"input"`
	Gas     uint64      `json:"gas"`
	Value   string      `json:"value"`
	Time    uint64      `json:"time"`
	Ret     string      `json:"ret"`
	Status  string      `json:"status"`
	GasUsed uint64      `json:"gasUsed"`
	Refund  int64       `json:"refund"`
	Logs    []goldenLog `json:"logs"`
}
