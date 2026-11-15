#!/bin/sh
# Fail if a tracked file contains a name this project does not use.
# The pattern is assembled at runtime so this script does not store it.
set -e
cd "$(dirname "$0")/.."
a=mura
b=tori
c=ca
d=sey
e=hand
f=made
g=mol
h=ly
i=roc
j=ket
pat="$a$b|$c$d|$e$f|$e $f|$g$h $i$j"
if git grep -niE -I "$pat" -- . ':(exclude)scripts/check-forbidden-names.sh'; then
    echo "a tracked file contains a forbidden name" >&2
    exit 1
fi
