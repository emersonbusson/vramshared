#!/usr/bin/env bash
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/cascade-preflight.sh
# Legacy direct preflight is disabled staging.  It formerly included a root
# module-load path, so even observation is not performed until a separately
# sealed, attended lifecycle transaction is designed and approved.
set -euo pipefail

printf 'CASCADE_PREFLIGHT=STAGING_REFUSED\n'
printf 'CASCADE_PREFLIGHT_REASON=direct_preflight_disabled_requires_future_sealed_attended_transaction\n'
printf '%s\n' 'No module, device, service, or cascade command was invoked.'
exit 2
