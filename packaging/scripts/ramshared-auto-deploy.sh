#!/usr/bin/env bash
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: packaging/scripts/ramshared-auto-deploy.sh
# Retired boot-time auto-deploy entry point. An attended release handoff must
# prove binary identity and drain active NBD swap before replacing a daemon.
set -euo pipefail

echo 'RamShared auto-deploy is disabled: use the attended release handoff and verify BINARY_MATCH before installation.' >&2
exit 1
