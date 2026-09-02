#!/bin/sh
# GIT_EDITOR for `git commit --fixup=amend:<target>`: writes the amend! subject git needs,
# then the target's NEW full message from a file. `--fixup=amend:` refuses `-m`, so this is
# how the replacement message gets in without an editor.
#
# usage: AMEND_SUBJECT="<target subject>" AMEND_MESSAGE=<file> GIT_EDITOR=amend-editor.sh \
#        git commit --fixup=amend:<target>
{ printf 'amend! %s\n\n' "$AMEND_SUBJECT"; cat "$AMEND_MESSAGE"; } > "$1"
