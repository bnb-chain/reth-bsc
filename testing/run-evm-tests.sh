#!/usr/bin/env bash
set -euo pipefail

# Configuration
EEST_VERSION="${EEST_VERSION:-v5.4.0}"
FIXTURES_URL="https://github.com/ethereum/execution-spec-tests/releases/download/${EEST_VERSION}/fixtures_stable.tar.gz"
TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/bsc-ef-tests"
FIXTURES_DIR="${TEST_DIR}/execution-spec-tests"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

log_info() {
    echo -e "${GREEN}[INFO]${NC} $1"
}

log_warn() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

log_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

# Download and extract test fixtures
download_fixtures() {
    if [[ -d "${FIXTURES_DIR}/blockchain_tests" ]]; then
        log_info "Test fixtures already exist at ${FIXTURES_DIR}"
        return 0
    fi

    log_info "Downloading execution spec test fixtures ${EEST_VERSION}..."
    mkdir -p "${FIXTURES_DIR}"

    local tmp_file
    tmp_file=$(mktemp)

    if ! curl -sL "${FIXTURES_URL}" -o "${tmp_file}"; then
        log_error "Failed to download fixtures from ${FIXTURES_URL}"
        rm -f "${tmp_file}"
        return 1
    fi

    log_info "Extracting fixtures..."
    if ! tar -xzf "${tmp_file}" -C "${FIXTURES_DIR}" --strip-components=1; then
        log_error "Failed to extract fixtures"
        rm -f "${tmp_file}"
        return 1
    fi

    rm -f "${tmp_file}"
    log_info "Fixtures downloaded and extracted to ${FIXTURES_DIR}"
}

# Run the tests
run_tests() {
    local filter="${1:-}"

    log_info "Running BSC EVM execution spec tests..."

    cd "${TEST_DIR}"

    local cmd="cargo test --release --features ef-tests"

    if [[ -n "${filter}" ]]; then
        cmd="${cmd} -- ${filter}"
    fi

    log_info "Running: ${cmd}"

    if ${cmd}; then
        log_info "All tests passed!"
        return 0
    else
        log_error "Some tests failed"
        return 1
    fi
}

# Run tests with nextest (faster parallel execution)
run_tests_nextest() {
    local filter="${1:-}"

    log_info "Running BSC EVM execution spec tests with nextest..."

    cd "${TEST_DIR}"

    local cmd="cargo nextest run --release --features ef-tests"

    if [[ -n "${filter}" ]]; then
        cmd="${cmd} -E 'test(${filter})'"
    fi

    log_info "Running: ${cmd}"

    if ${cmd}; then
        log_info "All tests passed!"
        return 0
    else
        log_error "Some tests failed"
        return 1
    fi
}

# Print usage
usage() {
    echo "Usage: $0 [command] [options]"
    echo ""
    echo "Commands:"
    echo "  download    Download test fixtures only"
    echo "  test        Run tests (downloads fixtures if needed)"
    echo "  nextest     Run tests with nextest (faster, downloads fixtures if needed)"
    echo "  clean       Remove downloaded fixtures"
    echo ""
    echo "Options:"
    echo "  --filter <pattern>   Run only tests matching the pattern"
    echo "  --version <version>  Use specific EEST version (default: ${EEST_VERSION})"
    echo ""
    echo "Examples:"
    echo "  $0 download"
    echo "  $0 test"
    echo "  $0 test --filter shanghai"
    echo "  $0 nextest --filter st_eip1559"
    echo ""
}

# Main
main() {
    local command="${1:-test}"
    shift || true

    local filter=""

    # Parse options
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --filter)
                filter="$2"
                shift 2
                ;;
            --version)
                EEST_VERSION="$2"
                FIXTURES_URL="https://github.com/ethereum/execution-spec-tests/releases/download/${EEST_VERSION}/fixtures_stable.tar.gz"
                shift 2
                ;;
            -h|--help)
                usage
                exit 0
                ;;
            *)
                log_error "Unknown option: $1"
                usage
                exit 1
                ;;
        esac
    done

    case "${command}" in
        download)
            download_fixtures
            ;;
        test)
            download_fixtures
            run_tests "${filter}"
            ;;
        nextest)
            download_fixtures
            run_tests_nextest "${filter}"
            ;;
        clean)
            log_info "Removing fixtures..."
            rm -rf "${FIXTURES_DIR}"
            log_info "Done"
            ;;
        -h|--help|help)
            usage
            exit 0
            ;;
        *)
            log_error "Unknown command: ${command}"
            usage
            exit 1
            ;;
    esac
}

main "$@"
