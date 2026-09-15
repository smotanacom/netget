#!/bin/bash
# E2E Test Runner for NetGet
# Runs E2E tests in isolation with proper feature gating
#
# Usage:
#   ./test-e2e.sh                    # Run all E2E tests
#   ./test-e2e.sh whois dns http     # Run specific protocol tests
#   ./test-e2e.sh --list             # List available protocols
#
# Features:
# - Validates feature gates exist in Cargo.toml
# - Uses cargo-isolated.sh for build isolation
# - Automatically sets OLLAMA_LOCK_PATH for concurrent test safety
# - Fails fast if invalid protocol specified

set -e

# Color output for better readability
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Determine project root
PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$PROJECT_ROOT"

# Parse arguments
PROTOCOLS=()
VERBOSE=false
DRY_RUN=false
USE_OLLAMA=false  # Default: use mocks (no Ollama)

# Process arguments
while [[ $# -gt 0 ]]; do
    case "$1" in
        --use-ollama)
            USE_OLLAMA=true
            shift
            ;;
        --verbose|-v)
            VERBOSE=true
            shift
            ;;
        --dry-run|-n)
            DRY_RUN=true
            shift
            ;;
        --help|-h)
            echo "E2E Test Runner for NetGet"
            echo ""
            echo "Usage:"
            echo "  $0 [OPTIONS] [PROTOCOLS...]"
            echo ""
            echo "Options:"
            echo "  --use-ollama     Use real Ollama instead of mocks (requires Ollama running)"
            echo "  --verbose, -v    Show detailed test output"
            echo "  --dry-run, -n    Show what would be run without executing"
            echo "  --help, -h       Show this help message"
            echo ""
            echo "Test Modes:"
            echo "  Default          Use mock LLM responses (no Ollama required)"
            echo "  --use-ollama     Use real Ollama (requires Ollama running)"
            echo ""
            echo "Examples:"
            echo "  $0                          # List available E2E test protocols"
            echo "  $0 all                      # Run all E2E tests with mocks"
            echo "  $0 amqp tcp                 # Run AMQP and TCP tests with mocks"
            echo "  $0 --use-ollama whois       # Run WHOIS tests with real Ollama"
            echo ""
            echo "For the scored real-model eval (pass rates, failure diagnoses, a"
            echo "published results file) see ./run-eval.sh, which is a different thing:"
            echo "this script runs the pass/fail suites, that one measures whether a"
            echo "model can drive a protocol from its own action descriptions."
            echo "  $0 --dry-run tor            # Preview what would be executed"
            exit 0
            ;;
        -*)
            echo -e "${RED}Error: Unknown option: $1${NC}" >&2
            echo "Use --help for usage information" >&2
            exit 1
            ;;
        *)
            PROTOCOLS+=("$1")
            shift
            ;;
    esac
done

# Function to extract available feature flags from Cargo.toml
get_available_features() {
    # Extract features from Cargo.toml (between [features] and next section)
    sed -n '/^\[features\]/,/^\[.*\]/p' "$PROJECT_ROOT/Cargo.toml" | \
        grep '^[a-z0-9_-]* =' | \
        cut -d' ' -f1 | \
        grep -v "^default$" | \
        grep -v "^all-protocols$" | \
        grep -v "^e2e-tests$" | \
        sort -u
}

# Function to get protocols with E2E tests and their feature gates
get_e2e_test_info() {
    # Find all directories in tests/server/ that contain e2e_test.rs
    # Output format: "test_name:feature1,feature2,..." (e.g., "torrent_integration:torrent-tracker,torrent-dht,torrent-peer")
    find "$PROJECT_ROOT/tests/server" -mindepth 2 -maxdepth 2 -name "e2e_test.rs" -type f | while read -r test_file; do
        protocol_dir=$(dirname "$test_file")
        test_name=$(basename "$protocol_dir")

        # Extract all features from cfg attributes in the test file
        # Look for patterns like: feature = "tor" or feature = "torrent-tracker"
        features=$(grep -o 'feature = "[^"]*"' "$test_file" | sed -E 's/feature = "([^"]+)"/\1/' | sort -u | tr '\n' ',' | sed 's/,$//')

        if [ -n "$features" ]; then
            echo "${test_name}:${features}"
        else
            # Fallback: assume test name matches feature name
            echo "${test_name}:${test_name}"
        fi
    done | sort -u
}

# Function to get unique features from E2E tests (expand multi-feature tests)
get_e2e_features() {
    get_e2e_test_info | cut -d: -f2 | tr ',' '\n' | sort -u
}

# Function to check if a feature exists in Cargo.toml
feature_exists() {
    local feature="$1"
    get_available_features | grep -q "^${feature}$"
}

# If no protocols specified, show list and exit
if [ ${#PROTOCOLS[@]} -eq 0 ]; then
    echo -e "${BLUE}Available E2E Test Features:${NC}"
    echo ""

    available_features=$(get_available_features)

    # Get unique features from E2E tests
    e2e_features=$(get_e2e_features)

    # Show features with their test directories
    for feature in $e2e_features; do
        # Get all test directories for this feature (handle multi-feature tests)
        tests=$(get_e2e_test_info | while IFS=: read -r test_name test_features; do
            if echo "$test_features" | grep -q "\b${feature}\b"; then
                echo -n "$test_name "
            fi
        done)

        if echo "$available_features" | grep -q "^${feature}$"; then
            echo -e "  ${GREEN}✓${NC} $feature ${YELLOW}($tests)${NC}"
        else
            echo -e "  ${RED}✗${NC} $feature ${YELLOW}(missing in Cargo.toml)${NC}"
        fi
    done

    echo ""
    echo -e "${BLUE}Total:${NC} $(echo "$e2e_features" | wc -l) unique features"
    echo ""
    echo -e "${BLUE}Usage:${NC}"
    echo "  $0 all                # Run all E2E tests"
    echo "  $0 whois dns tor      # Run specific protocol tests"
    exit 0
fi

# Handle "all" keyword to run all tests
if [ ${#PROTOCOLS[@]} -eq 1 ] && [ "${PROTOCOLS[0]}" = "all" ]; then
    echo -e "${BLUE}Running all E2E tests...${NC}"
    # Only include features that actually exist in Cargo.toml
    PROTOCOLS=()
    available_features=$(get_available_features)
    for feature in $(get_e2e_features); do
        if echo "$available_features" | grep -q "^${feature}$"; then
            PROTOCOLS+=("$feature")
        fi
    done
    echo -e "${BLUE}Found ${#PROTOCOLS[@]} valid E2E test features${NC}"
fi

# Validate all protocols have feature gates BEFORE running any tests
echo -e "${BLUE}Validating feature gates...${NC}"
INVALID_PROTOCOLS=()
for protocol in "${PROTOCOLS[@]}"; do
    if ! feature_exists "$protocol"; then
        INVALID_PROTOCOLS+=("$protocol")
    fi
done

if [ ${#INVALID_PROTOCOLS[@]} -gt 0 ]; then
    echo -e "${RED}Error: The following protocols do not have feature gates in Cargo.toml:${NC}" >&2
    for protocol in "${INVALID_PROTOCOLS[@]}"; do
        echo -e "  ${RED}✗${NC} $protocol" >&2
    done
    echo "" >&2
    echo "Available protocols:" >&2
    get_available_features | sed 's/^/  /' >&2
    exit 1
fi

# Validate all protocols have E2E test files
echo -e "${BLUE}Validating E2E test files...${NC}"
MISSING_TESTS=()
for protocol in "${PROTOCOLS[@]}"; do
    # Check if any test uses this feature (handle multi-feature tests)
    found=false
    while IFS=: read -r test_name test_features; do
        if echo "$test_features" | grep -q "\b${protocol}\b"; then
            found=true
            break
        fi
    done < <(get_e2e_test_info)

    if [ "$found" = false ]; then
        MISSING_TESTS+=("$protocol")
    fi
done

if [ ${#MISSING_TESTS[@]} -gt 0 ]; then
    echo -e "${RED}Error: The following features do not have E2E test files:${NC}" >&2
    for protocol in "${MISSING_TESTS[@]}"; do
        echo -e "  ${RED}✗${NC} $protocol" >&2
    done
    echo "" >&2
    echo "Available features with E2E tests:" >&2
    get_e2e_features | sed 's/^/  /' >&2
    exit 1
fi

echo -e "${GREEN}All validations passed!${NC}"
echo ""

# Export flag for tests if using Ollama
if [ "$USE_OLLAMA" = true ]; then
    export NETGET_USE_OLLAMA=1
    echo -e "${BLUE}Test Mode:${NC} Real (using Ollama)"
    echo -e "  ${YELLOW}→${NC} Using real Ollama (requires Ollama running)"
else
    echo -e "${BLUE}Test Mode:${NC} Mock (default)"
    echo -e "  ${YELLOW}→${NC} Using mock LLM responses (no Ollama required)"
fi
echo ""

# Setup Ollama lock for concurrent test safety
OLLAMA_LOCK_PATH="${OLLAMA_LOCK_PATH:-./netget-ollama.lock}"
export OLLAMA_LOCK_PATH
echo -e "${BLUE}Using Ollama lock:${NC} $OLLAMA_LOCK_PATH"
echo ""

# Track results
PASSED=0
FAILED=0
FAILED_PROTOCOLS=()

# Run tests for each protocol
for protocol in "${PROTOCOLS[@]}"; do
    echo -e "${BLUE}========================================${NC}"
    echo -e "${BLUE}Testing feature: ${GREEN}$protocol${NC}"
    echo -e "${BLUE}========================================${NC}"

    # Get all test files that use this feature
    test_files=()
    while IFS=: read -r test_name test_features; do
        # Check if this test requires only this feature OR multiple features including this one
        if echo "$test_features" | grep -q "\b${protocol}\b"; then
            # If test has multiple features, check if all are available
            all_features_present=true
            for req_feature in $(echo "$test_features" | tr ',' ' '); do
                if ! feature_exists "$req_feature"; then
                    all_features_present=false
                    break
                fi
            done

            if [ "$all_features_present" = true ]; then
                test_files+=("$test_name")
            fi
        fi
    done < <(get_e2e_test_info)

    if [ ${#test_files[@]} -eq 0 ]; then
        echo -e "${YELLOW}⚠ No runnable tests found for $protocol (tests may require additional features)${NC}"
        continue
    fi

    echo -e "${BLUE}Test files:${NC} ${test_files[*]}"

    # Build the test command
    # For multi-feature tests, we need to enable all required features
    all_required_features=""
    for test_file in "${test_files[@]}"; do
        test_info=$(get_e2e_test_info | grep "^${test_file}:")
        test_features=$(echo "$test_info" | cut -d: -f2)
        all_required_features="${all_required_features},${test_features}"
    done
    # Remove duplicates and leading comma
    all_required_features=$(echo "$all_required_features" | tr ',' '\n' | sort -u | tr '\n' ',' | sed 's/^,//' | sed 's/,$//')

    echo -e "${BLUE}Features enabled:${NC} $all_required_features"
    echo ""

    TEST_CMD=(
        ./cargo-isolated.sh
        test
        --no-default-features
        --features "$all_required_features"
    )

    # Real-Ollama mode travels in NETGET_USE_OLLAMA (exported above), never as a
    # test argument.
    #
    # This script used to append `-- --use-ollama`, and that did not merely fail
    # to help — it broke the mode it was advertising. libtest parses everything
    # after `--` itself and rejects an unknown flag outright:
    #
    #     $ <test-binary> --use-ollama
    #     error: Unrecognized option: 'use-ollama'
    #
    # so the process exited before a single test ran, and the script reported the
    # protocol FAILED. `./test-e2e.sh --use-ollama <protocol>` could not work in
    # that form. `tests/helpers/netget.rs` documents the same trap, and the arg
    # scan it keeps is only for a custom harness that forwards unknown flags.
    if [ "$VERBOSE" = true ]; then
        TEST_CMD+=(-- --nocapture --test-threads=1)
    fi

    # Run the test (or show what would be run in dry-run mode)
    if [ "$DRY_RUN" = true ]; then
        echo -e "${YELLOW}[DRY RUN]${NC} Would execute: ${TEST_CMD[*]}"
        echo -e "${YELLOW}[DRY RUN]${NC} Skipping actual test execution"
        PASSED=$((PASSED + 1))
    else
        if "${TEST_CMD[@]}" 2>&1; then
            echo -e "${GREEN}✓ $protocol E2E tests PASSED${NC}"
            PASSED=$((PASSED + 1))
        else
            echo -e "${RED}✗ $protocol E2E tests FAILED${NC}"
            FAILED=$((FAILED + 1))
            FAILED_PROTOCOLS+=("$protocol")
        fi
    fi
    echo ""
done

# Print summary
echo -e "${BLUE}========================================${NC}"
echo -e "${BLUE}E2E Test Summary${NC}"
echo -e "${BLUE}========================================${NC}"
echo -e "${GREEN}Passed:${NC} $PASSED"
if [ $FAILED -gt 0 ]; then
    echo -e "${RED}Failed:${NC} $FAILED"
    echo ""
    echo -e "${RED}Failed protocols:${NC}"
    for protocol in "${FAILED_PROTOCOLS[@]}"; do
        echo -e "  ${RED}✗${NC} $protocol"
    done
fi
echo ""

# Exit with appropriate code
if [ $FAILED -gt 0 ]; then
    exit 1
else
    echo -e "${GREEN}All E2E tests passed!${NC}"
    exit 0
fi
