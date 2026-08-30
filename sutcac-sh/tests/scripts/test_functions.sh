#!/usr/bin/env bash
# Test: function definitions and calls

# Basic function
greet() {
    echo "hello $1"
}
greet world

# Function with accumulator
sum() {
    total=0
    for n in $*; do
        ((total += n))
    done
    echo "$total"
}
sum 1 2 3 4 5

# Function status via last command
is_even() {
    (( $1 % 2 == 0 ))
}
if is_even 4; then echo "4 is even"; fi
if ! is_even 5; then echo "5 is odd"; fi

# Function calling another
outer() {
    inner "$@"
}
inner() {
    echo "inner got $# args"
}
outer a b c
