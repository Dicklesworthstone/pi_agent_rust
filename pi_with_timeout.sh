#!/bin/bash
# Pi Agent wrapper with increased timeout for Ollama Cloud models
#
# This wrapper sets PI_HTTP_REQUEST_TIMEOUT_SECS to 300 seconds (5 minutes)
# to prevent timeouts when using cloud-based models like glm-5.1:cloud
#
# Usage: ./pi_with_timeout.sh [pi arguments]
# Example: ./pi_with_timeout.sh -p "What is 2+2?"

export PI_HTTP_REQUEST_TIMEOUT_SECS=300

# Get the directory where this script is located
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Execute the pi binary with all passed arguments
exec "${SCRIPT_DIR}/target/release/pi" "$@"
