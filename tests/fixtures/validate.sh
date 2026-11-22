#!/bin/sh
# Validate schemas + example corpora. No network, no Python needed.
# Usage: sh tests/fixtures/validate.sh
#   - node tests/fixtures/validate.js   (schema parse, $ref resolution, example validation,
#                                        negative tests, regex regressions)
#   - node tests/fixtures/regex-checks.js
# validate.py is the jsonschema-library twin for CI environments that have it.
set -eu
cd "$(dirname "$0")/../.."
node tests/fixtures/validate.js
node tests/fixtures/regex-checks.js
