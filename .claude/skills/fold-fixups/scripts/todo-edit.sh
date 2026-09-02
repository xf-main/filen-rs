#!/bin/sh
# GIT_SEQUENCE_EDITOR helper: usage todo-edit.sh <action> <sha>[,<sha>...] <todo-file>
# Turns `pick <sha>` into `<action> <sha>` for each listed sha (short or full).
action=$1; shas=$2; todo=$3
for sha in $(printf '%s' "$shas" | tr ',' ' '); do
	short=$(git rev-parse --short "$sha")
	sed -i.bak -E "s/^pick ($short|$sha)/$action \1/" "$todo" && rm -f "$todo.bak"
done
