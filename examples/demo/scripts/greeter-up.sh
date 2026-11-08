#!/bin/sh
# Command fixture: creates some state and tells cargo-xtest about it.
set -e
state=$(mktemp)
echo hi > "$state"
printf '{"handle": "%s", "env": {"GREETER_STATE": "%s"}}\n' "$state" "$state"
