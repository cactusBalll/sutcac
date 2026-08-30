#!/usr/bin/env bash
# Test: shell builtins

# true / false / colon
: ; echo "colon=$?"
true; echo "true=$?"
false; echo "false=$?"

# cd / pwd
cd /tmp
echo "pwd=$(pwd)"
cd /var
echo "pwd2=$(pwd)"

# export
export FOO=bar
echo "FOO=$FOO"
export BAZ=qux
echo "BAZ=$BAZ"

# test builtin
if test -f /bin/sh; then echo "sh exists"; fi
if [ -d /tmp ]; then echo "tmp is dir"; fi
if [ "abc" = "abc" ]; then echo "strings equal"; fi
if [ 3 -lt 5 ]; then echo "3 lt 5"; fi

# shift
shift_func() {
    echo "all=$*"
    shift 2
    echo "after=$*"
}
shift_func 1 2 3 4
