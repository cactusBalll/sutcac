#!/usr/bin/env bash
# Test: special parameters and positional args

# $?
true
echo "true=$?"
false
echo "false=$?"

# $$ (pid) - just check it's a number
if echo "$$" | grep -qE '^[0-9]+$'; then echo "pid_is_numeric"; fi

# Function args
myfunc() {
    echo "count=$#"
    echo "first=$1"
    echo "second=$2"
    echo "all=$*"
}
myfunc alpha beta gamma

# Function status via last command
is_even() {
    (( $1 % 2 == 0 ))
}
if is_even 4; then echo "4 is even"; fi
if ! is_even 5; then echo "5 is odd"; fi

# Shift
shift_func() {
    echo "before=$#"
    shift
    echo "after=$#"
    echo "first=$1"
}
shift_func one two three

# Last status in connection
if true; then echo "last_status_zero"; fi
echo "$?"
