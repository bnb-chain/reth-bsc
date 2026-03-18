#!/bin/bash
# Test script for new RPC APIs (miner_*, eth_coinbase, eth_health)
# Usage: ./test_rpc_apis.sh [RPC_URL]
# Example: ./test_rpc_apis.sh http://localhost:8545

RPC_URL="${1:-http://localhost:8545}"
PASS=0
FAIL=0

green() { printf "\033[32m%s\033[0m" "$1"; }
red()   { printf "\033[31m%s\033[0m" "$1"; }

call_rpc() {
    local method="$1"
    local params="$2"
    curl -s -X POST "$RPC_URL" \
        -H "Content-Type: application/json" \
        -d "{\"jsonrpc\":\"2.0\",\"method\":\"$method\",\"params\":$params,\"id\":1}"
}

check() {
    local name="$1"
    local result="$2"
    local expect_field="$3"  # "result" or "error"
    local expect_match="$4"  # substring to match (optional)

    if [ -z "$result" ]; then
        printf "  %-50s %s\n" "$name" "$(red FAIL) - no response"
        FAIL=$((FAIL+1))
        return
    fi

    local has_field
    has_field=$(echo "$result" | python3 -c "import sys,json; d=json.load(sys.stdin); print('result' if 'result' in d else 'error')" 2>/dev/null)

    if [ "$has_field" != "$expect_field" ]; then
        printf "  %-50s %s\n" "$name" "$(red FAIL) - expected $expect_field"
        echo "    response: $result"
        FAIL=$((FAIL+1))
        return
    fi

    if [ -n "$expect_match" ]; then
        if echo "$result" | grep -q "$expect_match"; then
            printf "  %-50s %s\n" "$name" "$(green PASS)"
            PASS=$((PASS+1))
        else
            printf "  %-50s %s\n" "$name" "$(red FAIL) - missing '$expect_match'"
            echo "    response: $result"
            FAIL=$((FAIL+1))
        fi
    else
        printf "  %-50s %s\n" "$name" "$(green PASS)"
        PASS=$((PASS+1))
    fi
}

echo "============================================"
echo "  RPC API Test Suite"
echo "  Target: $RPC_URL"
echo "============================================"

# --- Connectivity check ---
R=$(call_rpc "web3_clientVersion" "[]")
if [ -z "$R" ] || echo "$R" | grep -q "Connection refused"; then
    echo "$(red 'Cannot connect to') $RPC_URL"
    exit 1
fi
CLIENT=$(echo "$R" | python3 -c "import sys,json; print(json.load(sys.stdin).get('result','unknown'))" 2>/dev/null)
echo "  Connected: $CLIENT"
echo ""

# ============================================
echo "--- eth_ namespace ---"
# ============================================

R=$(call_rpc "eth_coinbase" "[]")
check "eth_coinbase → returns address" "$R" "result" "0x"

R=$(call_rpc "eth_health" "[]")
check "eth_health → returns bool" "$R" "result"

R=$(call_rpc "eth_config" "[]")
check "eth_config → returns config object" "$R" "result" "current"

echo ""

# ============================================
echo "--- miner_ start/stop ---"
# ============================================

R=$(call_rpc "miner_stop" "[]")
check "miner_stop → ok" "$R" "result"

R=$(call_rpc "miner_start" "[]")
check "miner_start → ok" "$R" "result"

echo ""

# ============================================
echo "--- miner_ setExtra ---"
# ============================================

R=$(call_rpc "miner_setExtra" "[\"reth-bsc\"]")
check "miner_setExtra('reth-bsc') → true" "$R" "result" "true"

# extra data > 32 bytes should fail
R=$(call_rpc "miner_setExtra" "[\"this string is definitely longer than thirty two bytes!!\"]")
check "miner_setExtra(too long) → error" "$R" "error" "too long"

echo ""

# ============================================
echo "--- miner_ setGasPrice ---"
# ============================================

R=$(call_rpc "miner_setGasPrice" "[\"0x2540BE400\"]")
check "miner_setGasPrice(10gwei) → true" "$R" "result" "true"

echo ""

# ============================================
echo "--- miner_ setGasLimit ---"
# ============================================

R=$(call_rpc "miner_setGasLimit" "[140000000]")
check "miner_setGasLimit(140M) → true" "$R" "result" "true"

echo ""

# ============================================
echo "--- miner_ setEtherbase ---"
# ============================================

R=$(call_rpc "miner_setEtherbase" "[\"0x0000000000000000000000000000000000000001\"]")
check "miner_setEtherbase → true" "$R" "result" "true"

# Verify eth_coinbase reflects the change
R=$(call_rpc "eth_coinbase" "[]")
check "eth_coinbase reflects setEtherbase" "$R" "result" "0x0000000000000000000000000000000000000001"

echo ""

# ============================================
echo "--- miner_ setRecommitInterval ---"
# ============================================

R=$(call_rpc "miner_setRecommitInterval" "[3000]")
check "miner_setRecommitInterval(3000ms) → ok" "$R" "result"

echo ""

# ============================================
echo "--- miner_ MEV methods ---"
# ============================================

R=$(call_rpc "miner_mevRunning" "[]")
check "miner_mevRunning → returns bool" "$R" "result"

R=$(call_rpc "miner_startMev" "[]")
check "miner_startMev → ok" "$R" "result"

R=$(call_rpc "miner_mevRunning" "[]")
check "miner_mevRunning (after start) → true" "$R" "result" "true"

R=$(call_rpc "miner_stopMev" "[]")
check "miner_stopMev → ok" "$R" "result"

R=$(call_rpc "miner_mevRunning" "[]")
check "miner_mevRunning (after stop) → false" "$R" "result" "false"

echo ""

# ============================================
echo "--- miner_ builder management ---"
# ============================================

TEST_BUILDER="0x0000000000000000000000000000000000000001"

R=$(call_rpc "miner_addBuilder" "[\"$TEST_BUILDER\", \"https://test.example.com\"]")
check "miner_addBuilder → ok" "$R" "result"

R=$(call_rpc "mev_hasBuilder" "[\"$TEST_BUILDER\"]")
check "mev_hasBuilder (cross-check) → true" "$R" "result" "true"

R=$(call_rpc "miner_removeBuilder" "[\"$TEST_BUILDER\"]")
check "miner_removeBuilder → ok" "$R" "result"

R=$(call_rpc "mev_hasBuilder" "[\"$TEST_BUILDER\"]")
check "mev_hasBuilder (after remove) → false" "$R" "result" "false"

echo ""

# ============================================
echo "============================================"
printf "  Results: $(green "$PASS passed"), $(red "$FAIL failed")\n"
echo "============================================"

[ $FAIL -eq 0 ] && exit 0 || exit 1
