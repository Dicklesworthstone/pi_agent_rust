#!/usr/bin/env bash
# Scratch verification runner for agent sessions (routes cargo through rch).
set -u
cd /Users/jemanuel/projects/pi_agent_rust || exit 1
rch exec -- cargo "$@"
